//! Landing a run onto the user's branch (decision 0100, section 5), built on
//! merge-back's own merge computation.
//!
//! The merge is computed by [`MergeBack::compute_merge`] (`git merge-tree
//! --write-tree -z`, the same parser and the same case and normalization
//! collision check as merge-back), with the **target** as the first parent
//! and the candidate as the second: every land is one merge commit on the
//! target, even when a fast-forward would be possible. Nothing here touches
//! a working tree; a ref moves only by compare-and-swap (`update-ref <new>
//! <old>`). Moving a target branch that is checked out is the caller's step,
//! with the user's own git.

use super::safe_git::Target;
use super::{CAS_RETRIES, MergeBack, MergeError};
use crate::error::WorkspaceError;
use crate::signing::CommitSigning;

/// A landing merge computed in the object database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingMerge {
    /// The merged tree; `commit` is the merge commit (target first parent,
    /// candidate second), referenced by no ref yet.
    Clean { commit: String, tree: String },
    /// The candidate does not merge cleanly onto the target.
    Conflicted {
        paths: Vec<String>,
        /// Git's conflict labels, such as `CONFLICT (contents)`.
        kinds: Vec<String>,
    },
}

/// What [`MergeBack::fold_into_integration`] did to the integration branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fold {
    /// The landed merge commit is already on the branch: nothing to do (a
    /// resumed land, or a second call).
    AlreadyContains { tip: String },
    /// The branch tip was an ancestor of the merge commit (its tip was the
    /// landed candidate): the branch fast-forwarded to it.
    FastForwarded { from: String, to: String },
    /// Commits were verified after the candidate: the merge commit was merged
    /// into the branch, which keeps every one of them.
    Merged { from: String, commit: String },
    /// The merge commit does not merge cleanly into the branch; the branch
    /// did not move.
    Conflicted { tip: String, paths: Vec<String> },
}

impl MergeBack {
    /// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, MergeError> {
        let args = ["merge-base", "--is-ancestor", ancestor, descendant];
        let out = self.git.exec(&Target::Repo, &[], args, None)?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self
                .git
                .failed(&Target::Repo, &args.map(std::ffi::OsString::from), &out)),
        }
    }

    /// The commit `rev` names, or `None` when it names none (an unborn
    /// branch, a missing ref).
    pub fn resolve_commit(&self, rev: &str) -> Result<Option<String>, MergeError> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.git.exec(
            &Target::Repo,
            &[],
            ["rev-parse", "--verify", "--quiet", &spec],
            None,
        )?;
        Ok((out.code == Some(0)).then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }

    /// The tree of `commit`.
    pub fn tree_of(&self, commit: &str) -> Result<String, MergeError> {
        self.git.ok(
            &Target::Repo,
            &[],
            ["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
            None,
        )
    }

    /// Build the landing merge of `candidate` onto `target_tip`: one merge
    /// commit with the target as first parent, made as the owner identity
    /// merge-back was opened with, signed as `signing` says. No ref moves.
    ///
    /// A signed commit that git cannot sign is refused
    /// ([`MergeError::SigningFailed`]) with git's message.
    pub fn landing_merge(
        &self,
        target_tip: &str,
        candidate: &str,
        message: &str,
        signing: &CommitSigning,
    ) -> Result<LandingMerge, MergeError> {
        let computed = self.compute_merge(target_tip, candidate)?;
        if !computed.is_clean() {
            return Ok(LandingMerge::Conflicted {
                paths: computed
                    .paths
                    .into_iter()
                    .map(|p| p.path)
                    .chain(computed.collisions.into_iter().flatten())
                    .collect(),
                kinds: computed
                    .messages
                    .into_iter()
                    .map(|m| m.kind)
                    .filter(|k| k.starts_with("CONFLICT"))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            });
        }
        let env = self.identity_env();
        let flag = signing.commit_flag();
        let args = [
            "commit-tree",
            flag.as_str(),
            &computed.tree,
            "-p",
            target_tip,
            "-p",
            candidate,
            "-m",
            message,
        ];
        let out = self.git.exec(&Target::Repo, &env, args, None)?;
        if out.code != Some(0) {
            if signing.is_signed() {
                return Err(MergeError::SigningFailed { stderr: out.stderr });
            }
            return Err(self
                .git
                .failed(&Target::Repo, &args.map(std::ffi::OsString::from), &out));
        }
        let commit = String::from_utf8(out.stdout)
            .map(|s| s.trim().to_string())
            .map_err(|e| WorkspaceError::GitOutput {
                args: "commit-tree".into(),
                detail: e.to_string(),
            })?;
        Ok(LandingMerge::Clean {
            commit,
            tree: computed.tree,
        })
    }

    /// Move `refs/heads/<branch>` from `old` to `new` by compare-and-swap.
    /// `Ok(false)`: the branch is no longer at `old` (another process moved
    /// it); nothing changed. For a branch no worktree has checked out; the
    /// caller checks that first.
    pub fn move_branch(
        &self,
        branch: &str,
        new: &str,
        old: &str,
        reason: &str,
    ) -> Result<bool, MergeError> {
        self.git.ok(
            &Target::Repo,
            &[],
            ["check-ref-format", "--branch", branch],
            None,
        )?;
        let full = format!("refs/heads/{branch}");
        let out = self.git.exec(
            &Target::Repo,
            &[],
            ["update-ref", "-m", reason, &full, new, old],
            None,
        )?;
        if out.code == Some(0) {
            return Ok(true);
        }
        match self.resolve_commit(&full)? {
            Some(now) if now != old => Ok(false),
            _ => Err(self
                .git
                .failed(&Target::Repo, &["update-ref".into(), full.into()], &out)),
        }
    }

    /// Bring a landed merge commit onto the integration branch, so the branch
    /// never moves backward and drops no verified commit. Idempotent: a
    /// branch that already contains `merge_commit` is left alone.
    ///
    /// - tip an ancestor of `merge_commit` (the tip was the landed
    ///   candidate): fast-forward to it;
    /// - otherwise: merge `merge_commit` into the tip (tip first parent), an
    ///   internal unsigned commit, by compare-and-swap, recomputed when
    ///   another process moved the branch meanwhile;
    /// - a conflict leaves the branch where it is.
    pub fn fold_into_integration(&self, merge_commit: &str) -> Result<Fold, MergeError> {
        let _serial = self.merging.lock().map_err(|_| MergeError::Poisoned)?;
        self.refuse_checked_out()?;
        let full = self.full_ref();
        for _ in 0..CAS_RETRIES {
            let tip = self.integration_tip()?;
            if self.is_ancestor(merge_commit, &tip)? {
                return Ok(Fold::AlreadyContains { tip });
            }
            let (new, fold) = if self.is_ancestor(&tip, merge_commit)? {
                (
                    merge_commit.to_owned(),
                    Fold::FastForwarded {
                        from: tip.clone(),
                        to: merge_commit.to_owned(),
                    },
                )
            } else {
                let computed = self.compute_merge(&tip, merge_commit)?;
                if !computed.is_clean() {
                    return Ok(Fold::Conflicted {
                        tip,
                        paths: computed
                            .paths
                            .into_iter()
                            .map(|p| p.path)
                            .chain(computed.collisions.into_iter().flatten())
                            .collect(),
                    });
                }
                let message = format!("Merge landed {merge_commit} back into {}", self.branch);
                let env = self.identity_env();
                let commit = self.git.ok(
                    &Target::Repo,
                    &env,
                    [
                        "commit-tree",
                        "--no-gpg-sign",
                        &computed.tree,
                        "-p",
                        &tip,
                        "-p",
                        merge_commit,
                        "-m",
                        &message,
                    ],
                    None,
                )?;
                (
                    commit.clone(),
                    Fold::Merged {
                        from: tip.clone(),
                        commit,
                    },
                )
            };
            let moved = self.git.exec(
                &Target::Repo,
                &[],
                [
                    "update-ref",
                    "-m",
                    "kgraf: landed merge folded back",
                    &full,
                    &new,
                    &tip,
                ],
                None,
            )?;
            if moved.code == Some(0) {
                return Ok(fold);
            }
            if self.integration_tip()? == tip {
                return Err(self.git.failed(
                    &Target::Repo,
                    &["update-ref".into(), full.clone().into()],
                    &moved,
                ));
            }
        }
        Err(MergeError::Contended {
            branch: self.branch.clone(),
            tries: CAS_RETRIES,
        })
    }
}
