//! `apply_patch` with protected writes (decision 0094): in the Bypass mode, a
//! hunk on `.git/<path>` or on one of kGraf's settings files is written to the
//! place [`crate::protected::ProtectedWrites::admit`] resolves it to, behind
//! that guard's snapshot and integrity check; a hunk on `.gitattributes`,
//! `.gitmodules` or `.githooks/...` is applied in the worktree like an
//! ordinary file (with any ordinary hunks of the same patch), with no
//! snapshot. Every other mode, and every other protected path, is refused as
//! before. The tool's name, description
//! and schema are [`ApplyPatchTool`]'s, byte for byte, so the shared prefix
//! does not change with the mode (decision 0043).

use super::{
    ApplyPatchTool, Hunk, apply_all_names, envelope_supplied, internal, lexical, noting_envelope,
    parse, refuse_repeated_paths, written_paths,
};
use crate::fs_safety;
use crate::protected::{Area, ProtectedWrites, Target};
use crate::{Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// [`ApplyPatchTool`] bound to one attempt's [`ProtectedWrites`].
#[derive(Debug, Clone)]
pub struct ProtectedApplyPatch {
    guard: Arc<ProtectedWrites>,
}

impl ApplyPatchTool {
    /// This tool, with the attempt's protected-write guard.
    pub fn with_protected_writes(guard: Arc<ProtectedWrites>) -> ProtectedApplyPatch {
        ProtectedApplyPatch { guard }
    }
}

/// One protected hunk: rewritten to its target's relative path, the target,
/// and the path as the worker named it.
struct ProtectedHunk {
    hunk: Hunk,
    target: Target,
    shown: String,
    /// False for an ordinary file applied beside `.gitattributes`,
    /// `.gitmodules` or `.githooks` ([`Area::Tracked`]): not recorded as a
    /// protected write.
    protected: bool,
}

fn refused(path: &std::path::Path, why: &str) -> ToolError {
    ToolError::Access {
        tool: "apply_patch".into(),
        source: crate::AccessError::Escapes {
            path: format!("{}: {why}", path.display()),
        },
    }
}

impl ProtectedApplyPatch {
    /// `None` when no hunk names a protected path (the plain tool applies);
    /// the protected hunks otherwise. A refused path, a patch that mixes
    /// protected and ordinary paths, and a move involving a protected path
    /// are refused.
    fn split(&self, hunks: &[Hunk]) -> Result<Option<Vec<ProtectedHunk>>, ToolError> {
        let mut plain = 0usize;
        let mut plain_hunks = Vec::new();
        let mut tracked = Vec::new();
        let mut out = Vec::new();
        for hunk in hunks {
            let paths = written_paths(hunk)?;
            let mut admitted = Vec::new();
            for path in &paths {
                match self.guard.admit(path) {
                    None => plain += 1,
                    Some(Err(why)) => return Err(refused(path, &why)),
                    Some(Ok(target)) => admitted.push((path.clone(), target)),
                }
            }
            if admitted.is_empty() {
                plain_hunks.push((paths[0].clone(), hunk.clone()));
                continue;
            }
            // `.gitattributes`, `.gitmodules` and `.githooks` are ordinary
            // tracked files: the hunk applies as written, in the worktree,
            // deletions and moves included (git history is their snapshot).
            if admitted.iter().all(|(_, t)| t.area == Area::Tracked) {
                let (shown, target) = admitted.remove(0);
                tracked.push(ProtectedHunk {
                    hunk: hunk.clone(),
                    target,
                    shown: shown.display().to_string(),
                    protected: true,
                });
                continue;
            }
            // Deleting inside `.git` is a critical-path deletion, which asks
            // in every mode when the shell does it (decision 0094, point 4);
            // a patch cannot ask, so it is refused.
            if matches!(hunk, Hunk::DeleteFile { .. }) {
                return Err(refused(
                    &paths[0],
                    "deleting a protected path is refused; a deletion inside `.git` asks in \
                     every mode",
                ));
            }
            if paths.len() > 1 {
                return Err(ToolError::InvalidInput {
                    tool: "apply_patch".into(),
                    reason: "a move to or from a protected path is refused; add and delete in \
                             separate hunks"
                        .into(),
                });
            }
            let (shown, target) = admitted.remove(0);
            let rel = target.rel.clone();
            let hunk = match hunk.clone() {
                Hunk::AddFile { contents, .. } => Hunk::AddFile {
                    path: rel,
                    contents,
                },
                Hunk::DeleteFile { .. } => Hunk::DeleteFile { path: rel },
                Hunk::UpdateFile { chunks, .. } => Hunk::UpdateFile {
                    path: rel,
                    move_path: None,
                    chunks,
                },
            };
            out.push(ProtectedHunk {
                hunk,
                target,
                shown: shown.display().to_string(),
                protected: true,
            });
        }
        if !tracked.is_empty() && out.is_empty() {
            // Ordinary files in the same patch apply with them, in one pass
            // over the worktree.
            let root = tracked[0].target.root.clone();
            for (path, hunk) in plain_hunks {
                tracked.push(ProtectedHunk {
                    hunk,
                    target: Target {
                        area: Area::Tracked,
                        root: root.clone(),
                        rel: path.clone(),
                    },
                    shown: path.display().to_string(),
                    protected: false,
                });
            }
            return Ok(Some(tracked));
        }
        out.extend(tracked);
        match (plain, out.is_empty()) {
            (_, true) => Ok(None),
            (0, false) => Ok(Some(out)),
            _ => Err(ToolError::InvalidInput {
                tool: "apply_patch".into(),
                reason: "this patch mixes protected paths (`.git`, kGraf's settings) with \
                         ordinary files; send the protected hunks in a patch of their own"
                    .into(),
            }),
        }
    }
}

/// Snapshot, write, check, in that order, on a blocking thread. Returns the
/// tool's output: an error output (seen by the model) when a snapshot cannot
/// be taken, a hunk does not apply, or a check fails and the snapshot is put
/// back; the guard records the failure for the attempt in the last case.
fn apply_protected(
    guard: &ProtectedWrites,
    hunks: Vec<ProtectedHunk>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let mut areas: Vec<Area> = Vec::new();
    for h in &hunks {
        if !areas.contains(&h.target.area) {
            areas.push(h.target.area.clone());
        }
    }
    // Each window (for git: the repository lock) stays open until every
    // write in it is checked, below.
    let mut windows = Vec::new();
    for area in &areas {
        match guard.before_write(area) {
            Ok(w) => windows.push(w),
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "the protected write was not made: {e}"
                )));
            }
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for h in &hunks {
        if !roots.contains(&h.target.root) {
            roots.push(h.target.root.clone());
        }
    }
    let mut problems = Vec::new();
    for root in &roots {
        let group: Vec<&ProtectedHunk> = hunks.iter().filter(|h| &h.target.root == root).collect();
        let names = if group.iter().any(|h| h.target.area == Area::Tracked) {
            fs_safety::Names::BypassGitFiles
        } else {
            fs_safety::Names::Plain
        };
        let group: Vec<Hunk> = group.iter().map(|h| h.hunk.clone()).collect();
        if let Err(e) = apply_all_names(&group, root, cancel, names) {
            problems.push(e.to_string());
        }
    }
    // Every touched area is checked, whether or not its hunks applied: a
    // failure part way must not leave an unchecked write behind.
    let mut written = Vec::new();
    for h in &hunks {
        if !h.protected {
            written.push(h.shown.clone());
            continue;
        }
        match guard.after_write(&h.target.area, &h.shown) {
            Ok(()) => written.push(h.shown.clone()),
            Err(e) => problems.push(e),
        }
    }
    drop(windows);
    if problems.is_empty() {
        let checked = if hunks.iter().any(|h| h.target.area != Area::Tracked) {
            ", each `.git` or settings write checked after the write"
        } else {
            ""
        };
        Ok(ToolOutput::text(format!(
            "Success. Wrote these paths (Bypass mode){checked}:\n{}",
            written.join("\n")
        )))
    } else {
        Ok(ToolOutput::error(problems.join("\n")))
    }
}

#[async_trait::async_trait]
impl Tool for ProtectedApplyPatch {
    fn name(&self) -> &str {
        ApplyPatchTool.name()
    }

    fn definition(&self) -> ToolDefinition {
        ApplyPatchTool.definition()
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        ApplyPatchTool.permission_subject(input)
    }

    /// [`ApplyPatchTool::access`], with a protected path the guard admits
    /// declared as a write instead of refused.
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let hunks = parse(input)?;
        refuse_repeated_paths(&hunks, lexical)?;
        let mut access = Access::none();
        for hunk in &hunks {
            for path in written_paths(hunk)? {
                match self.guard.admit(&path) {
                    None => fs_safety::refuses_git_or_kgraf("apply_patch", &path)?,
                    Some(Ok(_)) => {}
                    Some(Err(why)) => return Err(refused(&path, &why)),
                }
                access = access
                    .write_path(&path)
                    .map_err(|source| ToolError::Access {
                        tool: "apply_patch".into(),
                        source,
                    })?;
            }
        }
        Ok(access)
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let hunks = parse(&input)?;
        let Some(protected) = self.split(&hunks)? else {
            return ApplyPatchTool.call(input, ctx, permit).await;
        };
        let supplied = envelope_supplied(&input);
        let guard = self.guard.clone();
        let cancel = ctx.cancel.clone();
        // Not raced against the cancel: a write that started must reach its
        // check, or it would be left unchecked.
        let result =
            tokio::task::spawn_blocking(move || apply_protected(&guard, protected, &cancel))
                .await
                .map_err(|e| internal(format!("apply task failed: {e}")))?;
        noting_envelope(result, supplied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancellationToken as CT;
    use crate::permissions::PermissionMode;
    use crate::protected::tests::{control, git, guard, repo};
    use serde_json::json;
    use std::path::Path;

    fn ctx(root: PathBuf) -> ToolContext {
        ToolContext {
            worker: "w1".into(),
            task: "t1".into(),
            workspace_root: root,
            cancel: CT::new(),
            progress: crate::ProgressSink::default(),
        }
    }

    fn wrap(body: &str) -> Value {
        json!({ "patch": format!("*** Begin Patch\n{body}\n*** End Patch") })
    }

    async fn run(tool: &ProtectedApplyPatch, input: Value, root: &Path) -> ToolOutput {
        tool.access(&input).expect("access");
        tool.call(input, &ctx(root.to_path_buf()), &CallPermit::new())
            .await
            .expect("call")
    }

    fn bypass_tool(r: &crate::protected::tests::Repo) -> ProtectedApplyPatch {
        ApplyPatchTool::with_protected_writes(Arc::new(guard(r, control(PermissionMode::Bypass))))
    }

    /// In Bypass, `.git/info/exclude`, a new ref and `.kgraf/run.json` land
    /// where git and kGraf read them, each checked and logged.
    #[tokio::test]
    async fn bypass_writes_git_and_settings_files() {
        let r = repo();
        let c = control(PermissionMode::Bypass);
        let tool = ApplyPatchTool::with_protected_writes(Arc::new(guard(&r, c.clone())));
        let exclude = std::fs::read_to_string(r.common.join("info/exclude")).expect("exclude");
        let last = exclude.lines().last().expect("a line").to_owned();
        let out = run(
            &tool,
            wrap(&format!(
                "*** Update File: .git/info/exclude\n@@\n {last}\n+/scratch/"
            )),
            &r.worktree,
        )
        .await;
        assert!(!out.is_error, "{}", out.render());
        assert!(
            std::fs::read_to_string(r.common.join("info/exclude"))
                .expect("exclude")
                .ends_with("/scratch/\n")
        );
        let head = git(&r.checkout, &["rev-parse", "main"]);
        let out = run(
            &tool,
            wrap(&format!("*** Add File: .git/refs/heads/feature\n+{head}")),
            &r.worktree,
        )
        .await;
        assert!(!out.is_error, "{}", out.render());
        assert_eq!(git(&r.checkout, &["rev-parse", "feature"]), head);
        let out = run(
            &tool,
            wrap("*** Add File: .kgraf/run.json\n+{\"routes\": []}"),
            &r.worktree,
        )
        .await;
        assert!(!out.is_error, "{}", out.render());
        assert!(r.state.join("run.json").is_file());
        assert!(!r.worktree.join(".kgraf").exists());
        let kept: Vec<String> = c
            .events_since(0)
            .into_iter()
            .map(|(_, e)| e.line())
            .collect();
        assert_eq!(kept.len(), 3, "{kept:?}");
        assert!(
            kept.iter()
                .all(|l| l.ends_with(": kept") && l.contains("Bypass")),
            "{kept:?}"
        );
    }

    /// A patch that turns a ref into garbage is undone, the call reports the
    /// check, and the attempt's guard holds the failure.
    #[tokio::test]
    async fn a_corrupting_patch_is_restored_and_reported() {
        let r = repo();
        let g = Arc::new(guard(&r, control(PermissionMode::Bypass)));
        let tool = ApplyPatchTool::with_protected_writes(g.clone());
        let sha = git(&r.checkout, &["rev-parse", "work"]);
        let out = run(
            &tool,
            wrap(&format!(
                "*** Update File: .git/refs/heads/work\n@@\n-{sha}\n+not-a-sha"
            )),
            &r.worktree,
        )
        .await;
        assert!(out.is_error);
        let text = out.render();
        assert!(text.contains("integrity check"), "{text}");
        assert_eq!(git(&r.checkout, &["rev-parse", "work"]), sha);
        git(
            &r.checkout,
            &["fsck", "--connectivity-only", "--no-dangling"],
        );
        assert!(g.failure().is_some());
    }

    #[tokio::test]
    async fn bypass_refuses_the_ledger_and_the_trace() {
        let r = repo();
        let tool = bypass_tool(&r);
        for p in [
            ".kgraf/ledger.db",
            ".kgraf/runs/r1/trace.jsonl",
            ".kgraf/graph.db",
        ] {
            let input = wrap(&format!("*** Add File: {p}\n+x"));
            let err = tool.access(&input).expect_err(p).to_string();
            assert!(err.contains("books"), "{p}: {err}");
            let err = tool
                .call(input, &ctx(r.worktree.clone()), &CallPermit::new())
                .await
                .expect_err(p);
            assert!(err.to_string().contains("books"));
        }
        assert!(!r.state.join("ledger.db").exists());
    }

    #[tokio::test]
    async fn other_modes_refuse_every_protected_write() {
        let r = repo();
        for mode in [
            PermissionMode::Manual,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::OrchestratorDecides,
        ] {
            let tool = ApplyPatchTool::with_protected_writes(Arc::new(guard(&r, control(mode))));
            for p in [".git/info/exclude", ".kgraf/run.json"] {
                let input = wrap(&format!("*** Add File: {p}\n+x"));
                assert!(tool.access(&input).is_err(), "{mode:?} {p}");
                assert!(
                    tool.call(input, &ctx(r.worktree.clone()), &CallPermit::new())
                        .await
                        .is_err(),
                    "{mode:?} {p}"
                );
            }
        }
        assert!(!r.state.join("run.json").exists());
    }

    #[tokio::test]
    async fn a_patch_mixing_protected_and_ordinary_paths_is_refused() {
        let r = repo();
        let tool = bypass_tool(&r);
        let input = wrap("*** Add File: .kgraf/run.json\n+{}\n*** Add File: b.txt\n+b");
        let err = tool
            .call(input, &ctx(r.worktree.clone()), &CallPermit::new())
            .await
            .expect_err("mixed");
        assert!(err.to_string().contains("patch of their own"), "{err}");
        assert!(!r.worktree.join("b.txt").exists());
    }

    #[tokio::test]
    async fn a_protected_deletion_is_refused_in_bypass() {
        let r = repo();
        let tool = bypass_tool(&r);
        let input = wrap("*** Delete File: .git/refs/heads/other");
        let err = tool
            .call(input, &ctx(r.worktree.clone()), &CallPermit::new())
            .await
            .expect_err("refused");
        assert!(err.to_string().contains("deleting"), "{err}");
        assert!(r.common.join("refs/heads/other").is_file());
    }

    /// Ordinary paths go to the plain tool unchanged, and the definition is
    /// the plain tool's.
    #[tokio::test]
    async fn an_ordinary_patch_is_the_plain_tool() {
        let r = repo();
        let tool = bypass_tool(&r);
        let out = run(&tool, wrap("*** Add File: b.txt\n+b"), &r.worktree).await;
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(r.worktree.join("b.txt")).expect("b"),
            "b\n"
        );
        assert_eq!(tool.definition(), ApplyPatchTool.definition());
    }
}
