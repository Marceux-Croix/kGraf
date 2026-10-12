//! Tests of the `replace_in_file` tool, through its `Tool` interface only.

use super::{ApplyPatchTool, REPLACE_IN_FILE, ReplaceInFileTool};
use crate::CancellationToken as CT;
use crate::permissions::PermissionMode;
use crate::protected::tests::{control, guard, repo};
use crate::{CallPermit, Tool, ToolContext, ToolError};
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

fn input(path: &str, old: &str, new: &str) -> Value {
    json!({ "path": path, "old": old, "new": new })
}

/// Runs `access` then `call` on a fresh workspace holding `seed`. The result is
/// the rendered output or the error text.
async fn run(
    tool: &ReplaceInFileTool,
    seed: &[(&str, &str)],
    call: Value,
) -> (Result<String, String>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text) in seed {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("seed dir");
        }
        std::fs::write(path, text).expect("seed");
    }
    let out = run_in(tool, dir.path(), call).await;
    (out, dir)
}

async fn run_in(tool: &ReplaceInFileTool, root: &Path, call: Value) -> Result<String, String> {
    tool.access(&call).map_err(|e| e.to_string())?;
    tool.call(call, &ctx(root.to_path_buf()), &CallPermit::new())
        .await
        .map(|o| o.render())
        .map_err(|e| e.to_string())
}

fn read(dir: &tempfile::TempDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join(name)).expect("read")
}

#[tokio::test]
async fn one_match_is_replaced_and_the_rest_of_the_file_is_kept() {
    let file = "fn a() {\n    one();\n    two();\n}\n";
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("src/a.rs", file)],
        input("src/a.rs", "    one();\n", "    uno();\n    dos();\n"),
    )
    .await;
    let text = out.expect("applies");
    assert!(text.contains("M src/a.rs"), "{text}");
    assert_eq!(
        read(&dir, "src/a.rs"),
        "fn a() {\n    uno();\n    dos();\n    two();\n}\n"
    );
}

/// The match is on the text, not on whole lines: a part of a line is replaced
/// and the rest of the line is kept.
#[tokio::test]
async fn a_part_of_a_line_is_replaced() {
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.txt", "let x = 1;\n")],
        input("a.txt", "x = 1", "x = 2"),
    )
    .await;
    out.expect("applies");
    assert_eq!(read(&dir, "a.txt"), "let x = 2;\n");
}

#[tokio::test]
async fn no_match_is_an_error_with_the_count_and_the_nearest_line() {
    let file = "fn alpha() {\n    work();\n}\n";
    // A tab where the file has spaces.
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.rs", file)],
        input("a.rs", "\twork();", "\tdone();"),
    )
    .await;
    let err = out.expect_err("no match");
    assert!(err.contains("occurs 0 times in a.rs"), "{err}");
    assert!(err.contains("Line 2"), "{err}");
    assert!(err.contains("work();"), "{err}");
    assert_eq!(read(&dir, "a.rs"), file, "nothing may be written");

    // Several lines, one of them wrong: the first disagreement is named.
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.rs", file)],
        input("a.rs", "fn alpha() {\n    wrk();\n}", "x"),
    )
    .await;
    let err = out.expect_err("no match");
    assert!(err.contains("occurs 0 times"), "{err}");
    assert!(
        err.contains("Nearest candidate is line 1 (2/3 lines"),
        "{err}"
    );
    assert!(err.contains("expected '    wrk();'"), "{err}");
    assert_eq!(read(&dir, "a.rs"), file);

    // Nothing in the file resembles it.
    let (out, _dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.rs", file)],
        input("a.rs", "zzz", "x"),
    )
    .await;
    let err = out.expect_err("no match");
    assert!(err.contains("occurs 0 times"), "{err}");
    assert!(err.contains("resembles"), "{err}");
}

#[tokio::test]
async fn several_matches_are_an_error_with_the_count_and_the_lines() {
    let file = "x = 1\ny = 2\nx = 1\nz = 3\nx = 1\n";
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.py", file)],
        input("a.py", "x = 1", "x = 9"),
    )
    .await;
    let err = out.expect_err("ambiguous");
    assert!(err.contains("occurs 3 times in a.py"), "{err}");
    assert!(err.contains("lines 1, 3, 5"), "{err}");
    assert_eq!(read(&dir, "a.py"), file, "nothing may be written");

    // A text that overlaps itself counts each place.
    let (out, _dir) = run(
        &ReplaceInFileTool::default(),
        &[("b.txt", "aaa")],
        input("b.txt", "aa", "b"),
    )
    .await;
    assert!(
        out.expect_err("overlap").contains("occurs 2 times"),
        "overlapping occurrences are counted"
    );
}

#[tokio::test]
async fn the_same_old_and_new_text_is_refused() {
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.txt", "x\n")],
        input("a.txt", "x", "x"),
    )
    .await;
    assert!(out.expect_err("same").contains("same text"));
    assert_eq!(read(&dir, "a.txt"), "x\n");
}

/// A model writes `\n`; a CRLF file is matched and written with CRLF, and the
/// result says so.
#[tokio::test]
async fn a_crlf_file_is_matched_with_crlf_and_the_result_says_so() {
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.txt", "one\r\ntwo\r\nthree\r\n")],
        input("a.txt", "one\ntwo", "ONE\nTWO"),
    )
    .await;
    let text = out.expect("applies");
    assert!(text.contains("CRLF"), "{text}");
    assert_eq!(read(&dir, "a.txt"), "ONE\r\nTWO\r\nthree\r\n");
}

/// In a CRLF file every line break `new` writes is CRLF, whether or not `old`
/// has a line break, and a `new` that already has CRLF gains no extra `\r`.
#[tokio::test]
async fn new_text_gets_crlf_line_breaks_in_a_crlf_file_without_doubling() {
    let file = "one\r\ntwo\r\n";
    // `old` is one line; `new` adds a line break.
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.txt", file)],
        input("a.txt", "one", "one\nuno"),
    )
    .await;
    let text = out.expect("applies");
    assert!(text.contains("CRLF"), "{text}");
    assert_eq!(read(&dir, "a.txt"), "one\r\nuno\r\ntwo\r\n");
    // `new` carries CRLF already.
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.txt", file)],
        input("a.txt", "one\ntwo", "ONE\r\nTWO"),
    )
    .await;
    out.expect("applies");
    assert_eq!(read(&dir, "a.txt"), "ONE\r\nTWO\r\n");
    // `old` carries CRLF too.
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.txt", file)],
        input("a.txt", "one\r\ntwo", "ONE\r\nTWO"),
    )
    .await;
    out.expect("applies");
    assert_eq!(read(&dir, "a.txt"), "ONE\r\nTWO\r\n");
}

/// A missing file with an empty `old` is created with `new`, even when `new`
/// is empty too; an existing file with equal `old` and `new` is still refused.
#[tokio::test]
async fn an_empty_old_and_new_creates_an_empty_file() {
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[],
        input("empty.txt", "", ""),
    )
    .await;
    out.expect("creates");
    assert_eq!(read(&dir, "empty.txt"), "");
}

/// An error from the shared write path names this tool, not `apply_patch`.
#[cfg(unix)]
#[tokio::test]
async fn a_hard_linked_file_is_refused_in_the_name_of_replace_in_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside");
    let target = outside.path().join("keys");
    std::fs::write(&target, "k\n").expect("seed");
    std::fs::hard_link(&target, dir.path().join("keys")).expect("hard link");
    let err = run_in(
        &ReplaceInFileTool::default(),
        dir.path(),
        input("keys", "k", "x"),
    )
    .await
    .expect_err("refused");
    assert!(err.contains("replace_in_file"), "{err}");
    assert!(!err.contains("apply_patch"), "{err}");
    assert_eq!(std::fs::read_to_string(&target).expect("outside"), "k\n");
}

#[tokio::test]
async fn an_empty_old_creates_a_missing_file_and_never_overwrites_one() {
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[],
        input("new/dir/a.txt", "", "hello\n"),
    )
    .await;
    let text = out.expect("creates");
    assert!(text.contains("A new/dir/a.txt"), "{text}");
    assert_eq!(read(&dir, "new/dir/a.txt"), "hello\n");

    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.txt", "keep\n")],
        input("a.txt", "", "hello\n"),
    )
    .await;
    let err = out.expect_err("exists");
    assert!(err.contains("exists"), "{err}");
    assert!(err.contains("apply_patch"), "{err}");
    assert_eq!(read(&dir, "a.txt"), "keep\n");

    // Text to replace in a file that is not there points to creation.
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[],
        input("gone.txt", "x", "y"),
    )
    .await;
    let err = out.expect_err("missing");
    assert!(err.contains("does not exist"), "{err}");
    assert!(err.contains("empty `old`"), "{err}");
    assert!(!dir.path().join("gone.txt").exists());
}

#[test]
fn the_call_declares_its_one_written_path() {
    let access = ReplaceInFileTool::default()
        .access(&input("./src/../src/a.rs", "x", "y"))
        .expect("access");
    let written: Vec<_> = access.writes_paths().iter().cloned().collect();
    assert_eq!(written, vec![PathBuf::from("src/a.rs")]);
}

/// An escape is refused when the call is declared and again when it runs,
/// as `apply_patch` does, and nothing is written.
#[tokio::test]
async fn a_path_outside_the_workspace_is_refused_like_apply_patch() {
    let tool = ReplaceInFileTool::default();
    let dir = tempfile::tempdir().expect("tempdir");
    for path in ["../outside.txt", "a/../../outside.txt", "/etc/hosts"] {
        let call = input(path, "", "x");
        let err = tool.access(&call).expect_err(path);
        assert!(matches!(err, ToolError::Access { .. }), "{path}: {err}");
        let err = tool
            .call(call, &ctx(dir.path().to_path_buf()), &CallPermit::new())
            .await
            .expect_err(path);
        assert!(matches!(err, ToolError::Access { .. }), "{path}: {err}");
    }
    // The same paths are refused by `apply_patch`, with the same error kind.
    let patch =
        json!({"patch": "*** Begin Patch\n*** Add File: ../outside.txt\n+x\n*** End Patch"});
    assert!(matches!(
        ApplyPatchTool.access(&patch),
        Err(ToolError::Access { .. })
    ));
    // A path that is the workspace itself.
    assert!(matches!(
        tool.access(&input("a/..", "", "x")),
        Err(ToolError::InvalidInput { .. })
    ));
    assert!(
        !dir.path()
            .parent()
            .expect("parent")
            .join("outside.txt")
            .exists()
    );
}

/// A link inside the workspace that leads out is not followed.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_that_leaves_the_workspace_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("keys"), "secret\n").expect("seed");
    std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).expect("link");
    let err = run_in(
        &ReplaceInFileTool::default(),
        dir.path(),
        input("d/keys", "secret", "pwned"),
    )
    .await
    .expect_err("refused");
    assert!(!err.is_empty());
    assert_eq!(
        std::fs::read_to_string(outside.path().join("keys")).expect("read"),
        "secret\n"
    );
}

#[tokio::test]
async fn git_and_kgraf_paths_are_refused() {
    for path in [".git/config", ".git/hooks/pre-commit", ".kgraf/ledger.db"] {
        let (out, dir) = run(&ReplaceInFileTool::default(), &[], input(path, "", "x")).await;
        out.expect_err(path);
        assert!(!dir.path().join(path).exists(), "{path}");
    }
}

/// With the guard of a run, the paths it handles for `apply_patch` are refused
/// here too: the books are refused in every mode, and the files Bypass admits
/// to `apply_patch` are pointed to it.
#[tokio::test]
async fn protected_paths_are_refused_in_every_mode_with_a_pointer_to_apply_patch() {
    let r = repo();
    for mode in [
        PermissionMode::Manual,
        PermissionMode::AcceptEdits,
        PermissionMode::Auto,
        PermissionMode::OrchestratorDecides,
        PermissionMode::Bypass,
    ] {
        let tool = ReplaceInFileTool::with_protected_writes(Arc::new(guard(&r, control(mode))));
        for path in [
            ".git/info/exclude",
            ".kgraf/run.json",
            ".kgraf/ledger.db",
            ".gitattributes",
            ".githooks/pre-commit",
        ] {
            let call = input(path, "", "x");
            let err = tool.access(&call).expect_err(path).to_string();
            assert!(!err.is_empty(), "{mode:?} {path}");
            let err = tool
                .call(call, &ctx(r.worktree.clone()), &CallPermit::new())
                .await
                .expect_err(path)
                .to_string();
            if mode == PermissionMode::Bypass
                && (path == ".gitattributes" || path == ".githooks/pre-commit")
            {
                assert!(err.contains("apply_patch"), "{path}: {err}");
            }
        }
    }
    assert!(!r.state.join("ledger.db").exists());
    assert!(!r.state.join("run.json").exists());
    assert!(!r.worktree.join(".gitattributes").exists());
    // A plain file is still edited with the guard in place.
    std::fs::write(r.worktree.join("a.txt"), "a\n").expect("seed");
    let tool = ReplaceInFileTool::with_protected_writes(Arc::new(guard(
        &r,
        control(PermissionMode::Bypass),
    )));
    run_in(&tool, &r.worktree, input("a.txt", "a", "b"))
        .await
        .expect("an ordinary file is edited");
    assert_eq!(
        std::fs::read_to_string(r.worktree.join("a.txt")).expect("read"),
        "b\n"
    );
}

/// The syntax check runs on every edit: a Python or YAML file that parsed
/// before and would not after is refused and left as it was. A file that was
/// already broken, and a kind with no check, are not blocked.
#[tokio::test]
async fn the_syntax_check_refuses_an_edit_that_breaks_a_python_or_yaml_file() {
    let py = "def f():\n    x = 1\n    return x\n";
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("m.py", py)],
        input("m.py", "    x = 1\n", "        x = 1\n"),
    )
    .await;
    let err = out.expect_err("breaks the file");
    assert!(err.contains("invalid Python"), "{err}");
    assert!(err.contains("first error at line"), "{err}");
    assert!(err.contains("not written"), "{err}");
    assert_eq!(read(&dir, "m.py"), py);

    let yaml = "a:\n  - 1\n  - 2\n";
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("c.yaml", yaml)],
        input("c.yaml", "  - 1\n", "  - [1\n"),
    )
    .await;
    assert!(out.expect_err("breaks yaml").contains("invalid YAML"));
    assert_eq!(read(&dir, "c.yaml"), yaml);

    // A valid edit is written.
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("m.py", py)],
        input("m.py", "x = 1", "x = 2"),
    )
    .await;
    out.expect("valid edit");
    assert_eq!(read(&dir, "m.py"), "def f():\n    x = 2\n    return x\n");

    // Already broken before the edit: not blocked.
    let broken = "def f(:\n    x = 1\n";
    let (out, dir) = run(
        &ReplaceInFileTool::default(),
        &[("b.py", broken)],
        input("b.py", "x = 1", "x = 2"),
    )
    .await;
    out.expect("a file that did not parse is not held to it");
    assert_eq!(read(&dir, "b.py"), "def f(:\n    x = 2\n");

    // No check for this kind.
    let (out, _dir) = run(
        &ReplaceInFileTool::default(),
        &[("a.rs", "fn a() {}\n")],
        input("a.rs", "fn a() {}", "fn a( {"),
    )
    .await;
    out.expect("no check for Rust");
}

#[tokio::test]
async fn an_already_cancelled_call_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), "x\n").expect("seed");
    let c = ctx(dir.path().to_path_buf());
    c.cancel.cancel();
    let err = ReplaceInFileTool::default()
        .call(input("a.txt", "x", "y"), &c, &CallPermit::new())
        .await
        .expect_err("cancelled");
    assert!(matches!(err, ToolError::Cancelled { .. }));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
        "x\n"
    );
}

#[test]
fn malformed_input_is_refused() {
    let tool = ReplaceInFileTool::default();
    for bad in [
        json!({"path": "a", "old": "x"}),
        json!({"path": "a", "new": "x"}),
        json!({"old": "x", "new": "y"}),
        json!({"path": 3, "old": "x", "new": "y"}),
    ] {
        assert!(
            matches!(tool.access(&bad), Err(ToolError::InvalidInput { .. })),
            "{bad}"
        );
    }
}

/// The definition is short, names when to use the tool instead of
/// `apply_patch`, and requires all three fields.
#[test]
fn the_definition_says_when_to_use_it() {
    let def = ReplaceInFileTool::default().definition();
    assert_eq!(def.name, REPLACE_IN_FILE);
    assert!(def.description.len() < 900, "{}", def.description);
    assert!(def.description.contains("apply_patch"));
    assert!(def.description.contains("exactly once"));
    assert_eq!(def.input_schema["required"], json!(["path", "old", "new"]));
    assert_eq!(def.input_schema["additionalProperties"], json!(false));
}
