//! Decision 0071: text overwritten in graph.db's `meta` table (run records)
//! does not stay readable in the file's freed pages.

use kgraf_graph::GraphStore;

#[test]
fn an_overwritten_run_record_leaves_no_residue_in_the_file() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("graph.db");
    let marker = "OVERWRITTEN_RECORD_RESIDUE_7f3a";
    {
        let store = GraphStore::open(&path, dir.path()).expect("open");
        // Large enough to spill onto overflow pages, with the marker at the
        // end so it lands on one: the case `secure_delete = FAST` misses.
        let record = format!("{}{marker}", "x".repeat(20_000));
        store.set_meta("orchestrator/run/r1", &record).expect("set");
        store
            .set_meta("orchestrator/run/r1", "clean")
            .expect("overwrite");
        store.set_meta("orchestrator/run/r2", &record).expect("set");
        store.clear_meta("orchestrator/run/r2").expect("clear");
    }
    // The last connection closed, so the WAL is checkpointed into the file.
    let bytes = std::fs::read(&path).expect("read");
    let found = bytes.windows(marker.len()).any(|w| w == marker.as_bytes());
    assert!(!found, "the overwritten record is still in graph.db");
}

/// WAL frames hold old page images until a checkpoint moves
/// them and the WAL is truncated, not only until the last connection closes.
/// `clear_meta` checkpoints, so a cleared record is gone from both files while
/// the store is still open.
#[test]
fn a_cleared_run_record_leaves_the_wal_while_the_store_is_open() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("graph.db");
    let marker = "CLEARED_RECORD_IN_WAL_91c2";
    let store = GraphStore::open(&path, dir.path()).expect("open");
    let record = format!("{}{marker}", "y".repeat(20_000));
    store.set_meta("orchestrator/run/r1", &record).expect("set");
    store.clear_meta("orchestrator/run/r1").expect("clear");
    for file in [path.clone(), dir.path().join("graph.db-wal")] {
        let bytes = std::fs::read(&file).unwrap_or_default();
        let found = bytes.windows(marker.len()).any(|w| w == marker.as_bytes());
        assert!(!found, "{} still holds the cleared record", file.display());
    }
    drop(store);
}

/// A reader holding an older snapshot blocks the checkpoint `clear_meta` tries.
/// The call returns at once, and once the reader lets go a background pass
/// truncates the WAL, so the cleared record does not wait for the last
/// connection to close.
#[test]
fn a_cleared_record_leaves_the_wal_after_a_blocking_reader_lets_go() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("graph.db");
    let wal = dir.path().join("graph.db-wal");
    let marker = "CLEARED_BEHIND_READER_5d08";
    let holds = |file: &std::path::Path| {
        std::fs::read(file)
            .unwrap_or_default()
            .windows(marker.len())
            .any(|w| w == marker.as_bytes())
    };
    let store = GraphStore::open(&path, dir.path()).expect("open");
    store.set_meta("seed", "x").expect("seed");
    let reader =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("reader");
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM meta;")
        .expect("pin a snapshot");

    let record = format!("{}{marker}", "z".repeat(20_000));
    store.set_meta("orchestrator/run/r1", &record).expect("set");
    let started = std::time::Instant::now();
    store.clear_meta("orchestrator/run/r1").expect("clear");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "clear_meta waited for the reader"
    );
    assert!(
        holds(&wal),
        "the reader should have kept the cleared record's frames in the WAL"
    );

    reader.execute_batch("COMMIT;").expect("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while holds(&wal) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !holds(&wal) && !holds(&path),
        "the background pass never removed the cleared record"
    );
    drop(store);
}
