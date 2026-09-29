//! What the store guarantees when a write fails, when an index is opened for
//! reading, and when a reader and a writer share the file.

use kgraf_graph::index::Indexer;
use kgraf_graph::{GraphError, GraphStore, NodeKind, NodeRecord};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn write(root: &Path, rel: &str, body: &str) {
    let full = root.join(rel);
    fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
    fs::write(full, body).expect("write");
}

fn node(file_id: &str, path: &str, id: &str) -> NodeRecord {
    NodeRecord {
        id: id.to_owned(),
        file_id: file_id.to_owned(),
        path: path.to_owned(),
        kind: NodeKind::Function,
        name: id.to_owned(),
        qualified_name: id.to_owned(),
        parent_id: None,
        start_line: 1,
        end_line: 1,
        start_byte: 0,
        end_byte: 0,
        signature: String::new(),
        identity_stable: true,
    }
}

#[test]
fn a_failed_node_write_does_not_commit_the_new_hash() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "src/a.rs", "pub fn one() {}\n");
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    Indexer::new(&store).index().expect("index");
    let mut file = store.file("src/a.rs").expect("file").expect("indexed");
    let old_hash = file.content_hash.clone();
    let old_nodes = store.nodes_in_file("src/a.rs").expect("nodes");

    // Two nodes with one id: the second insert fails after the file row and the
    // first node were written.
    file.content_hash = "new-hash".to_owned();
    let dup = vec![
        node(&file.id, &file.path, "dup"),
        node(&file.id, &file.path, "dup"),
    ];
    assert!(store.replace_file_nodes(&file, &dup, &[], &[]).is_err());

    let after = store.file("src/a.rs").expect("file").expect("indexed");
    assert_eq!(
        after.content_hash, old_hash,
        "the hash committed without its nodes"
    );
    assert_eq!(store.nodes_in_file("src/a.rs").expect("nodes"), old_nodes);
}

#[test]
fn a_pass_that_fails_between_hash_and_nodes_reindexes_the_file_next_time() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("graph.db");
    let tree = dir.path().join("tree");
    write(&tree, "src/a.rs", "pub fn before() {}\n");
    let store = GraphStore::open(&db, &tree).expect("store");
    Indexer::new(&store).index().expect("first pass");

    // A trigger makes every node insert fail, standing in for a kill or an error
    // after the file row was written.
    let raw = rusqlite::Connection::open(&db).expect("raw");
    raw.execute_batch(
        "CREATE TRIGGER fail_nodes BEFORE INSERT ON nodes
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
    )
    .expect("trigger");
    write(&tree, "src/a.rs", "pub fn after() {}\n");
    assert!(Indexer::new(&store).index().is_err());

    raw.execute_batch("DROP TRIGGER fail_nodes;")
        .expect("drop trigger");
    let report = Indexer::new(&store).index().expect("second pass");
    assert_eq!(
        report.files_changed, 1,
        "the failed file must read as changed: {report:?}"
    );
    assert!(store.resolve_symbol("after").is_ok());
    assert!(store.resolve_symbol("before").is_err());
}

#[test]
fn read_opens_refuse_a_missing_index_and_create_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("state").join("graph.db");
    fs::create_dir_all(db.parent().expect("parent")).expect("mkdir");
    let error = GraphStore::open_read_only_checked(&db, dir.path())
        .err()
        .expect("err");
    assert!(matches!(error, GraphError::NoIndex { .. }), "{error:?}");
    assert!(error.to_string().contains("kgraf index"), "{error}");
    let error = GraphStore::open_existing_for_facts(&db, dir.path())
        .err()
        .expect("err");
    assert!(matches!(error, GraphError::NoIndex { .. }), "{error:?}");
    assert!(!db.exists(), "a read open created the database");
}

#[test]
fn a_read_only_open_refuses_another_schema_and_leaves_it_intact() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "src/a.rs", "pub fn keep() {}\n");
    let db = dir.path().join("graph.db");
    {
        let store = GraphStore::open(&db, dir.path()).expect("store");
        Indexer::new(&store).index().expect("index");
        store.set_meta("schema_version", "3").expect("meta");
    }
    let error = GraphStore::open_read_only_checked(&db, dir.path())
        .err()
        .expect("err");
    assert!(
        matches!(error, GraphError::SchemaMismatch { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("kgraf index"), "{error}");
    let raw = rusqlite::Connection::open(&db).expect("raw");
    let nodes: i64 = raw
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .expect("count");
    assert!(nodes > 0, "the mismatch opened a rebuild");
}

/// One writer doing routine saves while a reader holds an old snapshot open.
/// A waiting checkpoint made each `clear_meta` cost the whole busy timeout;
/// the routine path must neither wait nor fail with "database is locked".
#[test]
fn routine_saves_do_not_wait_on_a_reader_and_never_report_locked() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "src/a.rs", "pub fn f() {}\n");
    let db = dir.path().join("graph.db");
    let writer = GraphStore::open(&db, dir.path()).expect("writer");
    Indexer::new(&writer).index().expect("index");

    let reader =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("reader");
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM nodes;")
        .expect("pin a snapshot");

    let started = Instant::now();
    for i in 0..6 {
        let key = format!("run/{i}");
        writer
            .set_meta(&key, &"x".repeat(20_000))
            .unwrap_or_else(|e| panic!("save {i} failed: {e:?}"));
        writer
            .clear_meta(&key)
            .unwrap_or_else(|e| panic!("clear {i} failed: {e:?}"));
    }
    let elapsed = started.elapsed();
    eprintln!("12 routine writes beside a pinned reader: {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "12 routine writes took {elapsed:?} beside a reader"
    );
    // The blocked checkpoint reports itself instead of waiting or raising.
    assert!(!writer.checkpoint_wal_nowait().expect("nowait"));

    reader.execute_batch("COMMIT;").expect("release");
    // The store's own background retry may hold the checkpoint for a moment
    // after the reader lets go, so wait for a checkpoint to succeed rather
    // than asserting on the first attempt.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !writer
        .checkpoint_wal_nowait()
        .expect("nowait after release")
    {
        assert!(
            Instant::now() < deadline,
            "the checkpoint never completed after the reader let go"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
