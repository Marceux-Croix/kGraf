//! `kgraf index` and `kgraf plan` presume work from the roadmap (decision 0064),
//! run as the built binary. Fail loud: no roadmap is an error for the explicit
//! command and a stated line for the index, never a silent skip.

use std::path::Path;
use std::process::{Command, Output};

fn kgraf(root: &Path, args: &[&str]) -> Output {
    let mut all = vec!["-C", root.to_str().expect("utf-8 path")];
    all.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .args(all)
        // Never write to the user's real diagnostics file from a test:
        // every test that runs the real binary must set
        // this or `KGRAF_DIAG_PATH`.
        .env("KGRAF_DIAG_DISABLE", "1")
        .output()
        .expect("run kgraf")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn write(root: &Path, path: &str, text: &str) {
    let p = root.join(path);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, text).expect("write");
}

#[test]
fn no_roadmap_is_said_by_index_and_refused_by_plan() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "src/lib.rs", "pub fn a() {}\n");
    let index = kgraf(dir.path(), &["index"]);
    assert!(index.status.success(), "{index:?}");
    assert!(
        stdout(&index)
            .contains("presumed   none: no roadmap found (looked for roadmap, todo, plan"),
        "{}",
        stdout(&index)
    );

    let plan = kgraf(dir.path(), &["plan", "--from-roadmap"]);
    assert!(!plan.status.success(), "no roadmap must fail: {plan:?}");
    let err = String::from_utf8_lossy(&plan.stderr);
    assert!(err.contains("no roadmap found"), "{err}");
    assert!(err.contains("--source"), "{err}");
}

#[test]
fn index_presumes_the_roadmap_by_default_and_a_re_run_changes_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "src/parse.rs", "pub fn parse_line() {}\n");
    write(
        dir.path(),
        "docs/ROADMAP.md",
        "# R\n\n## Stages\n\n| Stage | Description | Status |\n|---|---|---|\n\
         | 1 | Parser in `src/parse.rs` | Complete |\n| 2 | Export | In progress |\n| 3 | Import | Not started |\n",
    );
    let first = kgraf(dir.path(), &["index"]);
    assert!(first.status.success(), "{first:?}");
    let out = stdout(&first);
    assert!(
        out.contains("roadmap    docs/ROADMAP.md: 1 status table(s) with 3 row(s)"),
        "{out}"
    );
    assert!(out.contains("5 new"), "{out}");
    assert!(
        out.contains("1 presumed done, 1 partly done, 1 remaining"),
        "{out}"
    );

    let second = stdout(&kgraf(dir.path(), &["index"]));
    assert!(second.contains("0 new, 0 updated, 5 unchanged"), "{second}");

    let skipped = stdout(&kgraf(dir.path(), &["index", "--no-presume"]));
    assert!(
        skipped.contains("presumed   skipped (--no-presume)"),
        "{skipped}"
    );

    let list = stdout(&kgraf(dir.path(), &["plan"]));
    assert!(
        list.contains("presumed done          Stage 1: Parser"),
        "{list}"
    );
    assert!(
        list.contains("presumed remaining     Stage 3: Import"),
        "{list}"
    );

    // A named source outside the project is refused.
    let bad = kgraf(
        dir.path(),
        &["plan", "--from-roadmap", "--source", "../x.md"],
    );
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("not a path inside the project"));
}

/// Live presumed items as `kgraf plan` prints them (gone and adopted ones print
/// another state word).
fn live_items(root: &Path) -> Vec<String> {
    stdout(&kgraf(root, &["plan"]))
        .lines()
        .filter(|l| l.starts_with("  presumed "))
        .map(str::to_owned)
        .collect()
}

/// A roadmap that moved or was deleted left every one of its nodes
/// live, because only discovered documents were read. A moved roadmap's old
/// nodes go and the new path's are live; a deleted one's all go.
#[test]
fn a_moved_or_deleted_roadmap_retires_its_nodes() {
    let dir = tempfile::tempdir().expect("dir");
    write(
        dir.path(),
        "TODO.md",
        "## Todo\n\n- [ ] one\n- [x] two\n- [ ] three\n",
    );
    assert!(kgraf(dir.path(), &["index"]).status.success());
    assert_eq!(live_items(dir.path()).len(), 3);

    // Moved: the old path's nodes go, the new path's arrive.
    std::fs::create_dir_all(dir.path().join("docs")).expect("docs");
    std::fs::rename(dir.path().join("TODO.md"), dir.path().join("docs/todo.md")).expect("mv");
    let out = stdout(&kgraf(dir.path(), &["index"]));
    assert!(
        out.contains("roadmap    TODO.md: no longer there (moved or deleted); 5 of its node(s)"),
        "root, component and three items: {out}"
    );
    assert_eq!(live_items(dir.path()).len(), 3, "not six");
    let list = stdout(&kgraf(dir.path(), &["plan"]));
    assert_eq!(list.matches("gone from its source").count(), 3, "{list}");

    // Deleted: nothing is live, nothing is deleted, and the next index says
    // nothing more about it.
    std::fs::remove_file(dir.path().join("docs/todo.md")).expect("rm");
    let out = stdout(&kgraf(dir.path(), &["index"]));
    assert!(
        out.contains("roadmap    docs/todo.md: no longer there"),
        "{out}"
    );
    assert!(out.contains("presumed   none: no roadmap found"), "{out}");
    assert!(live_items(dir.path()).is_empty());
    let list = stdout(&kgraf(dir.path(), &["plan"]));
    assert_eq!(list.matches("gone from its source").count(), 6, "{list}");
    let again = stdout(&kgraf(dir.path(), &["index"]));
    assert!(!again.contains("no longer there"), "{again}");
}

/// A roadmap named with `--source` that discovery does not find stays live
/// through a later `kgraf index`: only a missing file is retired.
#[test]
fn a_named_source_is_not_retired_by_an_index_that_does_not_discover_it() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "notes/goals.md", "## Goals\n\n- [ ] ship\n");
    let named = kgraf(
        dir.path(),
        &["plan", "--from-roadmap", "--source", "notes/goals.md"],
    );
    assert!(named.status.success(), "{named:?}");
    let out = stdout(&kgraf(dir.path(), &["index"]));
    assert!(!out.contains("no longer there"), "{out}");
    assert_eq!(live_items(dir.path()).len(), 1);
}

/// A roadmap that is not UTF-8 is a stated error line, the
/// other document is still read, and the index succeeds and is committed; the
/// explicit command fails. A FIFO named as a source is refused, not read
/// forever.
#[test]
fn an_unreadable_roadmap_does_not_fail_the_index_and_a_fifo_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "src/lib.rs", "pub fn a() {}\n");
    std::fs::write(dir.path().join("ROADMAP.md"), [0xff, 0xfe, 0x00, 0x41]).expect("write");
    write(dir.path(), "TODO.md", "- [ ] still read\n");
    let index = kgraf(dir.path(), &["index"]);
    assert!(index.status.success(), "{index:?}");
    let out = stdout(&index);
    assert!(
        out.contains("presumed   error: ROADMAP.md not read:")
            && out.contains("(the index was committed)"),
        "{out}"
    );
    assert!(out.contains("roadmap    TODO.md:"), "{out}");
    assert!(stdout(&kgraf(dir.path(), &["symbol", "a"])).contains("src/lib.rs"));
    let plan = kgraf(dir.path(), &["plan", "--from-roadmap"]);
    assert!(!plan.status.success(), "the explicit command fails loud");

    #[cfg(unix)]
    {
        let fifo = dir.path().join("pipe.md");
        let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        let root = dir.path().to_owned();
        std::thread::spawn(move || {
            let _ = tx.send(kgraf(
                &root,
                &["plan", "--from-roadmap", "--source", "pipe.md"],
            ));
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("a FIFO source must not hang");
        assert!(!out.status.success());
        assert!(stdout(&out).contains("not a regular file"), "{out:?}");
    }
}
