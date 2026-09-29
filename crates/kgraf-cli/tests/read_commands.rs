//! Commands that only read the index must never create it, rebuild it, or
//! write to it. Only `kgraf index` may.

use kgraf_graph::{GraphStore, Indexer};
use std::path::Path;
use std::process::{Command, Output};

const READ_COMMANDS: &[&[&str]] = &[
    &["symbol", "foo"],
    &["slice", "foo"],
    &["query", "kind=function"],
    &["partners", "src/lib.rs"],
    &["demand"],
];

fn kgraf(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .env("KGRAF_DIAG_DISABLE", "1")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run kgraf")
}

fn db(root: &Path) -> std::path::PathBuf {
    root.join(".kgraf").join("graph.db")
}

#[test]
fn a_read_command_on_an_unindexed_project_fails_and_creates_no_database() {
    for args in READ_COMMANDS {
        let dir = tempfile::tempdir().expect("dir");
        let out = kgraf(dir.path(), args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} succeeded: {stderr}");
        assert!(stderr.contains("kgraf index"), "{args:?}: {stderr}");
        assert!(!db(dir.path()).exists(), "{args:?} created an index");
    }
}

#[test]
fn a_read_command_on_another_schema_version_refuses_and_leaves_the_file_as_it_was() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn foo() {}\n").expect("write");
    std::fs::create_dir_all(dir.path().join(".kgraf")).expect("state");
    {
        let store = GraphStore::open(&db(dir.path()), dir.path()).expect("open");
        Indexer::new(&store).index().expect("index");
        store.set_meta("schema_version", "3").expect("meta");
    }
    let before = std::fs::read(db(dir.path())).expect("read");
    // The commands that record facts refuse too; none of them rebuilds.
    let with_writers = READ_COMMANDS
        .iter()
        .copied()
        .chain([&["plan"][..], &["cochange", "--commits", "5"][..]]);
    for args in with_writers {
        let out = kgraf(dir.path(), args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} succeeded: {stderr}");
        assert!(
            stderr.contains("schema version") && stderr.contains("kgraf index"),
            "{args:?}: {stderr}"
        );
    }
    assert_eq!(
        before,
        std::fs::read(db(dir.path())).expect("read"),
        "a read command changed the index file"
    );
}

#[test]
fn a_read_command_on_a_current_index_still_reads() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn foo() {}\n").expect("write");
    let out = kgraf(dir.path(), &["index", "--no-presume"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = kgraf(dir.path(), &["symbol", "foo"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("src/lib.rs"));
    let out = kgraf(dir.path(), &["slice", "foo"]);
    assert!(out.status.success());
}

#[test]
fn an_edited_file_makes_slice_fail_until_the_next_index() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn foo() {}\n").expect("write");
    assert!(
        kgraf(dir.path(), &["index", "--no-presume"])
            .status
            .success()
    );
    std::fs::write(dir.path().join("src/lib.rs"), "// new\npub fn foo() {}\n").expect("edit");
    let out = kgraf(dir.path(), &["slice", "foo"]);
    assert!(!out.status.success(), "a stale slice must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("kgraf index"));
    assert!(
        kgraf(dir.path(), &["index", "--no-presume"])
            .status
            .success()
    );
    assert!(kgraf(dir.path(), &["slice", "foo"]).status.success());
}

#[test]
fn index_prints_what_the_walk_skipped() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn foo() {}\n").expect("write");
    std::fs::write(dir.path().join("target/debug/x.rs"), "pub fn gen() {}\n").expect("write");
    std::fs::write(dir.path().join("src/bad.rs"), [0x66, 0xff, 0xfe, 0x0a]).expect("write");
    let out = kgraf(dir.path(), &["index", "--no-presume"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("skipped excluded directory target"),
        "{stdout}"
    );
    assert!(stdout.contains("too large      0"), "{stdout}");
    assert!(stdout.contains("not utf-8      1"), "{stdout}");
}

#[test]
fn an_ambiguous_symbol_is_refused_by_slice_with_its_candidates() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/a.rs"), "pub fn twin() {}\n").expect("write");
    std::fs::write(dir.path().join("src/b.rs"), "pub fn twin() {}\n").expect("write");
    assert!(
        kgraf(dir.path(), &["index", "--no-presume"])
            .status
            .success()
    );
    let out = kgraf(dir.path(), &["slice", "twin"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("ambiguous"), "{stderr}");
    assert!(
        stderr.contains("src/a.rs") && stderr.contains("src/b.rs"),
        "{stderr}"
    );
}
