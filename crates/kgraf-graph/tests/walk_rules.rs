//! What a walk indexes, skips, keeps and reports.

use kgraf_graph::{GraphStore, Indexer};
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, body: &str) {
    let full = root.join(rel);
    fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
    fs::write(full, body).expect("write");
}

fn fresh() -> (tempfile::TempDir, GraphStore) {
    let dir = tempfile::tempdir().expect("dir");
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    (dir, store)
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_keeps_the_files_indexed_under_it_and_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, store) = fresh();
    write(dir.path(), "sub/deep/a.rs", "pub fn guarded() {}\n");
    write(dir.path(), "top.rs", "pub fn top() {}\n");
    Indexer::new(&store).index().expect("first");

    let sub = dir.path().join("sub");
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).expect("chmod");
    let listing_blocked = fs::read_dir(&sub).is_err();
    let result = Indexer::new(&store).index();
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).expect("chmod back");
    if !listing_blocked {
        return; // running as a user who ignores permissions
    }
    match result {
        Err(kgraf_graph::GraphError::Io { path, .. }) => {
            assert_eq!(path, "sub", "the error names the relative path");
        }
        other => panic!("an unreadable directory must be reported: {other:?}"),
    }
    assert!(
        store.file("sub/deep/a.rs").expect("file").is_some(),
        "an unreadable directory deleted the files indexed under it"
    );
    assert!(store.resolve_symbol("guarded").is_ok(), "rows were deleted");
}

#[test]
fn build_and_dist_inside_source_are_indexed_and_only_the_root_ones_are_not() {
    let (dir, store) = fresh();
    write(dir.path(), "src/build/mod.rs", "pub fn in_src_build() {}\n");
    write(dir.path(), "src/dist/mod.rs", "pub fn in_src_dist() {}\n");
    write(dir.path(), "build/out.rs", "pub fn root_build() {}\n");
    write(dir.path(), "dist/out.rs", "pub fn root_dist() {}\n");
    write(
        dir.path(),
        "crates/x/target/gen.rs",
        "pub fn deep_target() {}\n",
    );
    write(
        dir.path(),
        "web/node_modules/dep.js",
        "function deepDep() {}\n",
    );
    write(
        dir.path(),
        "app/__pycache__/m.py",
        "def cached():\n    pass\n",
    );

    let report = Indexer::new(&store).index().expect("index");
    assert!(store.resolve_symbol("in_src_build").is_ok());
    assert!(store.resolve_symbol("in_src_dist").is_ok());
    for skipped in [
        "root_build",
        "root_dist",
        "deep_target",
        "deepDep",
        "cached",
    ] {
        assert!(
            store.resolve_symbol(skipped).is_err(),
            "{skipped} was indexed"
        );
    }
    assert_eq!(
        report.excluded_dirs_skipped,
        [
            "app/__pycache__",
            "build",
            "crates/x/target",
            "dist",
            "web/node_modules"
        ]
    );
}

#[test]
fn a_source_directory_named_build_keeps_its_rows_across_passes() {
    let (dir, store) = fresh();
    write(dir.path(), "src/build/mod.rs", "pub fn kept() {}\n");
    Indexer::new(&store).index().expect("first");
    let again = Indexer::new(&store).index().expect("second");
    assert_eq!(again.files_removed, 0, "{again:?}");
    assert_eq!(again.files_unchanged, 1);
}

#[test]
fn a_configured_path_pattern_excludes_that_directory_only() {
    let (dir, store) = fresh();
    write(dir.path(), "vendor/lib/a.rs", "pub fn vendored() {}\n");
    write(dir.path(), "src/vendor/lib/b.rs", "pub fn own() {}\n");
    Indexer::new(&store)
        .with_excluded_dirs(vec!["vendor/lib".to_owned()])
        .index()
        .expect("index");
    assert!(store.resolve_symbol("vendored").is_err());
    assert!(store.resolve_symbol("own").is_ok());
}

#[test]
fn oversized_files_are_counted() {
    let (dir, store) = fresh();
    write(dir.path(), "src/small.rs", "pub fn small() {}\n");
    write(
        dir.path(),
        "src/big.rs",
        &format!("// {}\n", "x".repeat(400)),
    );
    let report = Indexer::new(&store)
        .with_max_file_bytes(100)
        .index()
        .expect("index");
    assert_eq!(report.skipped_too_large, 1, "{report:?}");
    assert_eq!(report.files_seen, 1);
}

#[cfg(unix)]
#[test]
fn a_symlink_never_deletes_an_indexed_file_and_is_reported() {
    let (dir, store) = fresh();
    write(dir.path(), "src/a.rs", "pub fn linked() {}\n");
    write(dir.path(), "src/real.rs", "pub fn real() {}\n");
    Indexer::new(&store).index().expect("first");
    assert!(store.resolve_symbol("linked").is_ok());

    // `a.rs` becomes a link to the other file.
    fs::remove_file(dir.path().join("src/a.rs")).expect("rm");
    std::os::unix::fs::symlink(dir.path().join("src/real.rs"), dir.path().join("src/a.rs"))
        .expect("symlink");
    let report = Indexer::new(&store).index().expect("second");
    assert_eq!(report.files_removed, 0, "{report:?}");
    assert_eq!(report.symlinks_kept, ["src/a.rs"]);
    assert!(
        store.resolve_symbol("linked").is_ok(),
        "the link deleted rows"
    );
}

#[cfg(unix)]
#[test]
fn a_metadata_error_never_deletes_an_indexed_file_and_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, store) = fresh();
    write(dir.path(), "sub/a.rs", "pub fn guarded() {}\n");
    Indexer::new(&store).index().expect("first");

    // Without search permission on the directory, `stat` on its entries fails.
    fs::set_permissions(dir.path().join("sub"), fs::Permissions::from_mode(0o444)).expect("chmod");
    let stat_blocked = fs::metadata(dir.path().join("sub/a.rs")).is_err();
    let result = Indexer::new(&store).index();
    fs::set_permissions(dir.path().join("sub"), fs::Permissions::from_mode(0o755)).expect("chmod");
    if !stat_blocked {
        return; // running as a user who ignores permissions
    }
    let report = result.expect("a metadata error is reported, not fatal");
    assert_eq!(report.files_removed, 0, "{report:?}");
    assert_eq!(report.metadata_errors, ["sub/a.rs"]);
    assert!(store.resolve_symbol("guarded").is_ok(), "rows were deleted");
}

#[test]
fn a_file_that_becomes_non_utf8_loses_its_stale_rows_and_is_reported() {
    let (dir, store) = fresh();
    write(dir.path(), "src/a.rs", "pub fn stale() {}\n");
    Indexer::new(&store).index().expect("first");
    assert!(store.resolve_symbol("stale").is_ok());

    fs::write(dir.path().join("src/a.rs"), [0x66, 0x6e, 0xff, 0xfe, 0x0a]).expect("write");
    let report = Indexer::new(&store).index().expect("second");
    assert_eq!(report.skipped_not_utf8, 1);
    assert_eq!(report.non_utf8_removed, ["src/a.rs"]);
    assert!(
        store.resolve_symbol("stale").is_err(),
        "stale nodes stayed beside changed bytes"
    );
    assert!(store.file("src/a.rs").expect("file").is_none());

    // The next pass leaves it alone: nothing to keep, still not UTF-8.
    let third = Indexer::new(&store).index().expect("third");
    assert_eq!(third.files_removed, 0);
}

/// A repository with ignore rules at the root and in a subfolder, a hidden
/// folder that must stay indexed, and a `.git` folder that must not be.
fn ignore_fixture(root: &Path) {
    write(
        root,
        ".gitignore",
        "target/\nnode_modules/\ngenerated/\n*.gen.rs\n!keep.gen.rs\n",
    );
    write(root, "src/lib.rs", "pub fn listed_source() {}\n");
    write(root, "target/debug/out.rs", "pub fn ignored_target() {}\n");
    write(root, "node_modules/dep/i.js", "function ignoredDep() {}\n");
    write(root, "generated/g.rs", "pub fn ignored_generated() {}\n");
    write(root, "src/api.gen.rs", "pub fn ignored_pattern() {}\n");
    write(root, "src/keep.gen.rs", "pub fn negated_back_in() {}\n");
    write(root, "sub/.gitignore", "local_only.rs\n");
    write(
        root,
        "sub/local_only.rs",
        "pub fn ignored_nested_rule() {}\n",
    );
    write(root, "sub/shown.rs", "pub fn shown_nested() {}\n");
    write(root, ".git/info/exclude", "scratch/\n");
    write(root, "scratch/s.rs", "pub fn ignored_by_exclude() {}\n");
    write(root, ".git/hooks/h.rs", "pub fn inside_dot_git() {}\n");
    write(
        root,
        ".github/scripts/ci.py",
        "def hidden_folder_source():\n    pass\n",
    );
}

#[test]
fn the_index_honors_gitignore_and_keeps_hidden_folders() {
    let (dir, store) = fresh();
    ignore_fixture(dir.path());
    Indexer::new(&store).index().expect("index");

    for listed in [
        "listed_source",
        "negated_back_in",
        "shown_nested",
        "hidden_folder_source",
    ] {
        assert!(
            store.resolve_symbol(listed).is_ok(),
            "{listed} should be indexed"
        );
    }
    for skipped in [
        "ignored_target",
        "ignoredDep",
        "ignored_generated",
        "ignored_pattern",
        "ignored_nested_rule",
        "ignored_by_exclude",
        "inside_dot_git",
    ] {
        assert!(
            store.resolve_symbol(skipped).is_err(),
            "{skipped} should not be indexed"
        );
    }
    assert!(store.file(".github/scripts/ci.py").expect("file").is_some());
}

#[test]
fn a_file_that_becomes_ignored_loses_its_rows() {
    let (dir, store) = fresh();
    write(dir.path(), "gen/a.rs", "pub fn soon_ignored() {}\n");
    Indexer::new(&store).index().expect("first");
    assert!(store.resolve_symbol("soon_ignored").is_ok());

    write(dir.path(), ".gitignore", "gen/\n");
    Indexer::new(&store).index().expect("second");
    assert!(store.resolve_symbol("soon_ignored").is_err());
}

#[test]
fn git_is_skipped_even_when_the_exclusion_list_is_replaced() {
    let (dir, store) = fresh();
    write(
        dir.path(),
        ".git/hooks/h.rs",
        "pub fn inside_dot_git() {}\n",
    );
    write(dir.path(), "src/a.rs", "pub fn plain() {}\n");
    Indexer::new(&store)
        .with_excluded_dirs(Vec::new())
        .index()
        .expect("index");
    assert!(store.resolve_symbol("plain").is_ok());
    assert!(store.resolve_symbol("inside_dot_git").is_err());
}

#[test]
fn a_rule_in_a_folder_above_the_root_does_not_hide_the_project() {
    // A project folder inside a repository that ignores its own location
    // (a linked worktree under an ignored folder) must still index.
    let (dir, _) = fresh();
    let project = dir.path().join("ignored_home/project");
    write(dir.path(), ".gitignore", "ignored_home/\n");
    write(&project, "src/a.rs", "pub fn project_source() {}\n");
    let store = GraphStore::open_in_memory(&project).expect("store");
    Indexer::new(&store).index().expect("index");
    assert!(store.resolve_symbol("project_source").is_ok());
}
