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
