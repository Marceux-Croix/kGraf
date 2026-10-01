//! `.gitattributes`, `.gitmodules` and `.githooks` through `apply_patch` with
//! the protected-write guard (decision 0094): written in the worktree like any
//! file in Bypass, deletions and moves included; refused in every other mode;
//! never through a link into `.git`.

use super::ApplyPatchTool;
use crate::CancellationToken as CT;
use crate::permissions::PermissionMode;
use crate::protected::Area;
use crate::protected::tests::{control, guard, repo};
use crate::{CallPermit, Tool, ToolContext, ToolOutput};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// `Err` when `access` refuses the call before it runs.
async fn run(
    mode: PermissionMode,
    root: &Path,
    r: &crate::protected::tests::Repo,
    body: &str,
) -> Result<ToolOutput, String> {
    let tool = ApplyPatchTool::with_protected_writes(Arc::new(guard(r, control(mode))));
    let input = wrap(body);
    tool.access(&input).map_err(|e| e.to_string())?;
    tool.call(input, &ctx(root.to_path_buf()), &CallPermit::new())
        .await
        .map_err(|e| e.to_string())
}

#[test]
fn bypass_admits_the_three_names_as_tracked_files_and_nothing_else() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    for p in [
        ".gitattributes",
        "sub/.gitattributes",
        ".gitmodules",
        ".githooks/pre-commit",
    ] {
        let t = g.admit(Path::new(p)).expect("protected").expect("admitted");
        assert_eq!(t.area, Area::Tracked, "{p}");
        assert_eq!(t.root, r.worktree, "{p}");
        assert_eq!(t.rel, PathBuf::from(p), "{p}");
    }
    for p in [
        ".gitconfig",
        "GITATT~1",
        ".githooks/.git/config",
        ".gitattributes::$DATA",
    ] {
        assert!(
            g.admit(Path::new(p)).expect("protected").is_err(),
            "{p} must stay refused in Bypass"
        );
    }
    for mode in [
        PermissionMode::Manual,
        PermissionMode::AcceptEdits,
        PermissionMode::Auto,
        PermissionMode::OrchestratorDecides,
    ] {
        let g = guard(&r, control(mode));
        for p in [".gitattributes", ".gitmodules", ".githooks/pre-commit"] {
            assert!(
                g.admit(Path::new(p)).expect("protected").is_err(),
                "{p} in {mode:?}"
            );
        }
    }
}

#[tokio::test]
async fn bypass_writes_moves_and_deletes_them_beside_ordinary_files() {
    let r = repo();
    let w = r.worktree.clone();
    let out = run(
        PermissionMode::Bypass,
        &w,
        &r,
        "*** Add File: .gitattributes\n+*.bin binary\n\
         *** Add File: .githooks/pre-commit\n+#!/bin/sh\n\
         *** Add File: .gitmodules\n+[submodule \"s\"]\n\
         *** Add File: notes.txt\n+n",
    )
    .await
    .expect("applied");
    assert!(!out.is_error, "{out:?}");
    assert_eq!(
        std::fs::read_to_string(w.join(".gitattributes")).unwrap(),
        "*.bin binary\n"
    );
    assert_eq!(
        std::fs::read_to_string(w.join(".githooks/pre-commit")).unwrap(),
        "#!/bin/sh\n"
    );
    assert_eq!(std::fs::read_to_string(w.join("notes.txt")).unwrap(), "n\n");
    let out = run(
        PermissionMode::Bypass,
        &w,
        &r,
        "*** Delete File: .gitmodules\n\
         *** Update File: .githooks/pre-commit\n*** Move to: .githooks/pre-push\n@@\n-#!/bin/sh\n+#!/bin/sh\n+exit 0",
    )
    .await
    .expect("applied");
    assert!(!out.is_error, "{out:?}");
    assert!(!w.join(".gitmodules").exists());
    assert!(!w.join(".githooks/pre-commit").exists());
    assert_eq!(
        std::fs::read_to_string(w.join(".githooks/pre-push")).unwrap(),
        "#!/bin/sh\nexit 0\n"
    );
}

#[tokio::test]
async fn another_mode_refuses_them_and_writes_nothing() {
    let r = repo();
    let w = r.worktree.clone();
    for mode in [
        PermissionMode::Manual,
        PermissionMode::AcceptEdits,
        PermissionMode::Auto,
    ] {
        let err = run(mode, &w, &r, "*** Add File: .gitattributes\n+* filter=x")
            .await
            .expect_err("refused");
        assert!(err.contains("steers git"), "{mode:?}: {err}");
        assert!(!w.join(".gitattributes").exists());
    }
}

/// A `.gitattributes` that is a link into `.git` is never written through.
#[cfg(unix)]
#[tokio::test]
async fn a_link_named_like_them_into_git_is_refused() {
    let r = repo();
    let w = r.worktree.clone();
    std::fs::create_dir_all(w.join("g")).unwrap();
    std::os::unix::fs::symlink(&r.common, w.join("g/real")).unwrap();
    std::os::unix::fs::symlink("g/real/config", w.join(".gitattributes")).unwrap();
    let before = std::fs::read_to_string(r.common.join("config")).unwrap();
    let out = run(
        PermissionMode::Bypass,
        &w,
        &r,
        "*** Update File: .gitattributes\n@@\n+[core]\n+\tfsmonitor = evil",
    )
    .await;
    // Refused at access or as the call's error output; never written.
    if let Ok(o) = out {
        assert!(o.is_error, "{o:?}");
    }
    assert_eq!(
        std::fs::read_to_string(r.common.join("config")).unwrap(),
        before
    );
}

/// A tracked git file is a worktree change: the guard records the write but
/// does not list it among the writes that land outside the worktree, so the
/// worker's change list keeps it for the scope checks.
#[tokio::test]
async fn a_tracked_git_file_stays_in_the_workers_change_list() {
    let r = repo();
    let g = Arc::new(guard(&r, control(PermissionMode::Bypass)));
    let tool = ApplyPatchTool::with_protected_writes(g.clone());
    let input = wrap("*** Add File: .gitattributes\n+* text=auto");
    tool.access(&input).expect("access");
    let out = tool
        .call(input, &ctx(r.worktree.clone()), &CallPermit::new())
        .await
        .expect("call");
    assert!(!out.is_error, "{out:?}");
    assert!(!g.wrote(Path::new(".gitattributes")));
}
