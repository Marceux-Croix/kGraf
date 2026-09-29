//! The live activity card's rows from a real change log (decision 0063): a real
//! index, a real lease through the real registry, the real tail thread, and the
//! route back to the graph through a model built from the real snapshot.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use kgraf_galaxy::ProjectModel;
use kgraf_graph::snapshot::{
    ProjectIndex, STATE_DIR, graph_db_path, now_ms, open_existing, read_snapshot,
};
use kgraf_graph::{GraphStore, Indexer, LeaseRegistry};
use kgraf_shell::activity::{self, ActivityUpdate, ActivityWatch, Target};

fn write(root: &Path, rel: &str, body: &str) {
    let full = root.join(rel);
    fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
    fs::write(full, body).expect("write");
}

/// Waits for the watch's next rows that satisfy `done`.
fn rows_until(
    watch: &ActivityWatch,
    what: &str,
    done: impl Fn(&[activity::ActivityRow]) -> bool,
) -> Vec<activity::ActivityRow> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "never: {what}");
        match watch.latest() {
            Some(ActivityUpdate::Rows(rows)) if done(&rows) => return rows.to_vec(),
            Some(ActivityUpdate::Unavailable(e)) => panic!("unavailable: {e}"),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[test]
fn the_card_lists_change_log_events_newest_first_and_routes_them_to_nodes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "pub fn alpha() { beta(); }\npub fn beta() {}\n",
    );
    fs::create_dir_all(root.join(STATE_DIR)).expect("state dir");
    let store = GraphStore::open(&graph_db_path(root), root).expect("store");
    Indexer::new(&store).index().expect("index");
    // A second connection writes behind the store's API: the triggers log it.
    let raw = rusqlite::Connection::open(graph_db_path(root)).expect("raw");
    // Index churn after the file rows: layout moves, as a reindex writes them
    // by the thousand (12,388 on this repository), and node updates.
    for i in 0..500 {
        raw.execute("UPDATE layout SET x = x + ?1", [f64::from(i) * 1e-6])
            .expect("layout");
    }
    raw.execute("UPDATE nodes SET signature = 'x'", [])
        .expect("nodes");
    let layout: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM change_log WHERE table_name = 'layout'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert!(layout > activity::ROWS as i64, "{layout} layout rows");

    let watch =
        ActivityWatch::spawn(root.to_path_buf(), Duration::from_millis(20), || {}).expect("watch");
    // The churn does not fill the tail: the file rows under it are shown.
    let rows = rows_until(&watch, "the index's events", |r| !r.is_empty());
    assert!(rows.len() <= activity::ROWS, "bounded");
    assert!(
        rows.windows(2).all(|w| w[0].seq > w[1].seq),
        "newest first, in time order only"
    );
    assert!(
        rows.iter()
            .all(|r| r.who == "indexer" && r.text.starts_with("indexed ")),
        "only file inserts from the index: {rows:?}"
    );

    // A worker takes a lease, then renews it: the take is at the top, and the
    // renewal is not a row.
    let before = rows[0].seq;
    let reg = LeaseRegistry::new(&store);
    let got = reg
        .acquire_all(
            "worker-7",
            "task-42",
            &["src/lib.rs".to_owned()],
            &[None],
            now_ms(),
        )
        .expect("lease");
    let kgraf_graph::Acquisition::Granted(leases) = got else {
        panic!("granted");
    };
    assert!(reg.renew(&leases[0], now_ms()).expect("renew"));
    let rows = rows_until(&watch, "the lease event", |r| {
        r.first().is_some_and(|top| top.seq > before)
    });
    let top = &rows[0];
    assert_eq!(top.who, "worker-7");
    assert_eq!(top.text, "took a lease on src/lib.rs");
    assert_eq!(top.target, Target::Agent("worker-7".into()));
    assert!(!rows.iter().any(|r| r.text.contains("renewed")), "{rows:?}");

    // The route back, computed once for the rows: the lease row selects the
    // agent's node, the file row the file's first code node.
    let ProjectIndex::Ready(read) = open_existing(root).expect("open") else {
        panic!("indexed");
    };
    let snapshot = read_snapshot(&read).expect("snapshot");
    let (model, _) = ProjectModel::build(&snapshot).expect("model");
    let slots = activity::routes(&rows, &model);
    assert_eq!(slots[0], model.agent_slot("worker-7"));
    let file = rows
        .iter()
        .position(|r| r.target == Target::Path("src/lib.rs".into()))
        .expect("the file row");
    let slot = slots[file].expect("the file is drawn");
    assert!(
        model
            .qualified_name(slot)
            .is_some_and(|q| q.contains("alpha") || q.contains("beta") || q.contains("lib"))
    );

    // Churn alone sends nothing (an idle card costs no frame).
    raw.execute("UPDATE layout SET x = x + 1e-6", [])
        .expect("layout");
    std::thread::sleep(Duration::from_millis(150));
    assert!(watch.latest().is_none(), "no update without a shown event");
}

#[test]
fn a_folder_without_an_index_is_said_not_hidden() {
    let dir = tempfile::tempdir().expect("tempdir");
    let watch = ActivityWatch::spawn(dir.path().to_path_buf(), Duration::from_millis(20), || {})
        .expect("watch");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no report");
        match watch.latest() {
            Some(ActivityUpdate::Unavailable(e)) => {
                assert!(e.contains("no index"), "{e}");
                break;
            }
            Some(ActivityUpdate::Rows(_)) => panic!("rows from nothing"),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
