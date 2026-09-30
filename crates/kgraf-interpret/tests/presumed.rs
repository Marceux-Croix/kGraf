//! Presumed work nodes (decision 0064), end to end against a fixture repository:
//! a roadmap with done, partial and not-started items, code for some of them, a
//! re-read, an edit, code landing, a model reading that cites the roadmap, and an
//! approval that adopts the presumed nodes. Each assertion is on the graph the
//! mechanism wrote or on the state it derives, not on a setting (test the mechanism, not the setting).

mod support;

use kgraf_graph::{EventTail, GraphStore, Indexer, WorkKind};
use kgraf_interpret::presumed::{self, PresumedState};
use kgraf_interpret::{Claim, NoTestRuns};
use serde_json::json;
use support::*;

const ROADMAP: &str = "# Demo roadmap\n\n## Phases\n\n\
| Milestone | Description | Status |\n|---|---|---|\n\
| 1 | Parser | Complete |\n\
| 2 | Export | IN PROGRESS (csv first) |\n\
| 3 | Import | Not started |\n\n\
## Phase detail\n\n\
### Milestone 1: Parser\n\nLives in `src/parse.rs`, entry point `parse_line()`.\n\n\
### Milestone 2: Export\n\n`src/export.rs` with `export_csv()`; `export_json()` is still to come.\n\n\
### Milestone 3: Import\n\nWill add `src/import.rs` and `import_csv()`.\n";

const P: &str = "presumed/roadmap/ROADMAP.md";

fn id(item: &str) -> String {
    format!("{P}/item/phases/{item}")
}

fn index(store: &GraphStore) {
    Indexer::new(store).index().expect("index");
}

fn presume(store: &GraphStore, now: i64) -> presumed::SourceReport {
    let mut r =
        presumed::presume_roadmaps(store, &["ROADMAP.md".to_owned()], now).expect("presume");
    r.remove(0)
}

fn state(store: &GraphStore, node: &str) -> presumed::PresumedView {
    presumed::views(store)
        .expect("views")
        .into_iter()
        .find(|v| v.id == node)
        .unwrap_or_else(|| panic!("no {node}"))
}

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, GraphStore) {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    write(dir.path(), "ROADMAP.md", ROADMAP);
    write(dir.path(), "src/parse.rs", "pub fn parse_line() {}\n");
    write(dir.path(), "src/export.rs", "pub fn export_csv() {}\n");
    let s = store(dir.path(), db.path());
    index(&s);
    (dir, db, s)
}

#[test]
fn an_existing_repository_gets_its_roadmap_as_presumed_nodes_reconciled_with_the_code() {
    let (_dir, _db, s) = fixture();
    assert_eq!(presumed::discover(s.root()), ["ROADMAP.md"]);
    let r = presume(&s, 100);
    assert_eq!((r.items, r.components), (3, 1));
    assert_eq!(r.sync.inserted.len(), 5, "root, one component, three items");

    let nodes = s.work_nodes(P).expect("nodes");
    assert_eq!(nodes[0].kind, WorkKind::Plan);
    assert_eq!(nodes[0].title, "Demo roadmap");
    let items: Vec<&str> = nodes
        .iter()
        .filter(|n| n.kind == WorkKind::Item)
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(items, [id("1"), id("2"), id("3")]);

    let one = state(&s, &id("1"));
    assert_eq!(
        (one.claim, one.state),
        (Claim::Complete, PresumedState::Done)
    );
    assert_eq!(one.title, "Milestone 1: Parser");
    assert_eq!(one.present, ["src/parse.rs", "parse_line"]);
    assert_eq!(one.parser, "kgraf roadmap-md/1");
    assert_eq!(one.source, "ROADMAP.md");
    assert_eq!(one.section, "Phases > row 1");

    let two = state(&s, &id("2"));
    assert_eq!(
        (two.claim, two.state),
        (Claim::Partial, PresumedState::Partial)
    );
    assert_eq!(two.missing, ["export_json"]);
    assert_eq!(two.claim_text, "IN PROGRESS (csv first)");

    let three = state(&s, &id("3"));
    assert_eq!(
        (three.claim, three.state),
        (Claim::NotStarted, PresumedState::Remaining)
    );

    let phases = state(&s, &format!("{P}/component/phases"));
    assert_eq!(phases.state, PresumedState::Partial, "a mix of its items");
}

#[test]
fn a_re_run_is_idempotent_and_a_changed_roadmap_updates_the_same_nodes() {
    let (dir, _db, s) = fixture();
    presume(&s, 100);
    let mut tail = EventTail::from_now(&s).expect("tail");
    let again = presume(&s, 200);
    assert!(!again.sync.wrote(), "{:?}", again.sync);
    assert_eq!(again.sync.unchanged, 5);
    assert!(
        tail.next_batch(&s, 100).expect("batch").is_empty(),
        "a re-run writes nothing"
    );

    // The roadmap moves on: milestone 3 starts, milestone 4 is added, milestone 2 is renamed.
    let moved = ROADMAP
        .replace(
            "| 3 | Import | Not started |",
            "| 3 | Import | IN PROGRESS |\n| 4 | Sync | Planned |",
        )
        .replace("| 2 | Export |", "| 2 | Export and share |");
    write(dir.path(), "ROADMAP.md", &moved);
    let r = presume(&s, 300);
    assert_eq!(r.sync.inserted, [id("4")]);
    // A new title rewrites the node; a new status word is the document's
    // claim, kept in the presumed row, so the node itself is not rewritten.
    assert_eq!(r.sync.updated, [id("2")]);
    assert_eq!(r.sync.claims_updated, [id("3")]);
    assert!(r.sync.gone.is_empty());
    assert_eq!(state(&s, &id("3")).state, PresumedState::Partial);
    assert_eq!(state(&s, &id("2")).title, "Milestone 2: Export and share");
    assert_eq!(
        s.work_nodes(P).expect("nodes").len(),
        6,
        "updated in place, never duplicated"
    );

    // A row removed from the roadmap is marked gone, not deleted.
    let fewer = moved.replace("| 4 | Sync | Planned |\n", "");
    write(dir.path(), "ROADMAP.md", &fewer);
    let r = presume(&s, 400);
    assert_eq!(r.sync.gone, [id("4")]);
    assert!(state(&s, &id("4")).gone);
    assert_eq!(s.work_nodes(P).expect("nodes").len(), 6);
}

#[test]
fn a_user_edit_survives_a_re_run() {
    let (dir, _db, s) = fixture();
    presume(&s, 100);
    let mut mine = s
        .work_nodes(P)
        .expect("nodes")
        .into_iter()
        .find(|n| n.id == id("2"))
        .expect("2");
    mine.title = "Export (edited by hand)".into();
    s.put_work(&[mine], &[]).expect("edit");

    write(
        dir.path(),
        "ROADMAP.md",
        &ROADMAP.replace(
            "| 2 | Export | IN PROGRESS (csv first) |",
            "| 2 | Export v2 | Complete |",
        ),
    );
    let r = presume(&s, 200);
    assert_eq!(r.sync.kept_edited, [id("2")]);
    let two = state(&s, &id("2"));
    assert_eq!(two.title, "Export (edited by hand)");
    assert!(two.edited);
    // The edit did not freeze what the roadmap says of it.
    assert_eq!(two.claim, Claim::Complete);
    assert_eq!(two.claim_text, "Complete");
}

#[test]
fn code_landing_moves_the_derived_state_and_a_contradiction_is_said() {
    let (dir, _db, s) = fixture();
    presume(&s, 100);
    // Work lands for milestone 3 before the roadmap says it started.
    write(dir.path(), "src/import.rs", "pub fn import_csv() {}\n");
    // And the parser file of the "complete" milestone 1 is deleted.
    std::fs::remove_file(dir.path().join("src/parse.rs")).expect("rm");
    index(&s);

    let three = state(&s, &id("3"));
    assert_eq!(three.state, PresumedState::Partial);
    assert!(
        three
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("says not started, but this exists")),
        "{three:?}"
    );
    let one = state(&s, &id("1"));
    assert_eq!(one.state, PresumedState::Partial);
    assert!(
        one.detail
            .as_deref()
            .is_some_and(|d| d.contains("says done, but this is not found: src/parse.rs")),
        "{one:?}"
    );
    // Derived, not stored: the nodes themselves did not change.
    assert!(!presume(&s, 200).sync.wrote());
}

#[tokio::test]
async fn a_reading_does_not_duplicate_the_roadmap_and_approval_adopts_the_presumed_nodes() {
    let (_dir, _db, s) = fixture();
    presume(&s, 100);

    let mut import = item(
        "import",
        &[],
        json!({"paths": ["src/import.rs"]}),
        "not_started",
        json!([{"text": "imports", "check": {"kind": "path", "path": "src/import.rs"}}]),
    );
    import["sources"] = json!(["ROADMAP.md#Milestone 3: Import"]);
    let docs = item(
        "docs",
        &[],
        json!({"paths": ["docs/guide.md"]}),
        "unstated",
        json!([{"text": "a guide exists", "check": {"kind": "path", "path": "docs/guide.md"}}]),
    );
    let model = FakeModel::new(move |_| answer(vec![import.clone(), docs.clone()], json!([]), &[]));
    let reading = run(&s, request(), &model, &NoTestRuns).await.expect("read");
    assert_eq!(model.calls(), 1);

    // The reading's items become presumed nodes, except the one a roadmap node
    // already stands for.
    let r = presumed::presume_reading(&s, &reading, 200).expect("presume reading");
    assert_eq!(r.covered, [("import".to_owned(), id("3"))]);
    let docs_node = "presumed/reading/item/item-docs".to_owned();
    assert!(r.sync.inserted.contains(&docs_node), "{:?}", r.sync);
    let d = state(&s, &docs_node);
    assert_eq!(d.state, PresumedState::Remaining);
    assert!(d.parser.contains("model fake:model"), "{}", d.parser);
    assert!(d.parser.contains("ledger call call-1"), "{}", d.parser);
    // Reading again with the same answer is idempotent.
    let again = presumed::presume_reading(&s, &reading, 250).expect("again");
    assert!(!again.sync.wrote(), "{:?}", again.sync);

    // Approval: the plan's items adopt the roadmap node they cite and the reading
    // node they came from. Both keep their ids.
    let plan = reading.plan_id().to_owned();
    let approved = reading.approve(&plan, &[]).expect("approve");
    let seeded = approved.seed(&s, 300).expect("seed");
    let mut adopted = seeded.adopted.clone();
    adopted.sort();
    assert_eq!(
        adopted,
        [
            (docs_node.clone(), format!("{plan}/item/docs")),
            (id("3"), format!("{plan}/item/import")),
        ]
    );
    assert_eq!(
        state(&s, &id("3")).adopted_by.as_deref(),
        Some(format!("{plan}/item/import").as_str())
    );

    // A re-read of the roadmap leaves the adopted node exactly as it is, and a
    // second seed adopts nothing again.
    let r = presume(&s, 400);
    assert_eq!(r.sync.kept_adopted, [id("3")]);
    assert!(approved.seed(&s, 500).expect("seed").adopted.is_empty());
    let ids: Vec<String> = presumed::views(&s)
        .expect("views")
        .into_iter()
        .map(|v| v.id)
        .collect();
    assert_eq!(
        ids.iter().filter(|i| **i == id("3")).count(),
        1,
        "one node, same identity"
    );
}

#[tokio::test]
async fn a_scoping_reading_with_questions_only_leaves_earlier_nodes_alone() {
    let (_dir, _db, s) = fixture();
    let first = FakeModel::new(|_| {
        answer(
            vec![item(
                "docs",
                &[],
                json!({}),
                "unstated",
                json!([{"text": "x"}]),
            )],
            json!([]),
            &[],
        )
    });
    let reading = run(&s, request(), &first, &NoTestRuns).await.expect("read");
    presumed::presume_reading(&s, &reading, 10).expect("presume");
    let before = presumed::views(&s).expect("views");

    let scoping = FakeModel::new(|_| answer(vec![], json!([]), &["What is it for?"]));
    let mut corrected = request();
    corrected.corrections = vec!["start over".into()];
    let reading = run(&s, corrected, &scoping, &NoTestRuns)
        .await
        .expect("read");
    let r = presumed::presume_reading(&s, &reading, 20).expect("presume");
    assert!(r.questions_only);
    assert!(!r.sync.wrote());
    assert_eq!(presumed::views(&s).expect("views"), before);
}

/// A roadmap node its source no longer lists is not adopted, even
/// when an approved item cites the heading it once stood for.
#[tokio::test]
async fn a_gone_roadmap_node_is_not_adopted() {
    let (dir, _db, s) = fixture();
    presume(&s, 100);
    write(
        dir.path(),
        "ROADMAP.md",
        &ROADMAP.replace("| 3 | Import | Not started |\n", ""),
    );
    assert_eq!(presume(&s, 150).sync.gone, [id("3")]);

    let mut import = item(
        "import",
        &[],
        json!({"paths": ["src/import.rs"]}),
        "not_started",
        json!([{"text": "imports", "check": {"kind": "path", "path": "src/import.rs"}}]),
    );
    import["sources"] = json!(["ROADMAP.md#Milestone 3: Import"]);
    let model = FakeModel::new(move |_| answer(vec![import.clone()], json!([]), &[]));
    let reading = run(&s, request(), &model, &NoTestRuns).await.expect("read");
    let r = presumed::presume_reading(&s, &reading, 200).expect("presume");
    assert!(r.covered.is_empty(), "a gone node covers nothing");
    let plan = reading.plan_id().to_owned();
    let seeded = reading
        .approve(&plan, &[])
        .expect("approve")
        .seed(&s, 300)
        .expect("seed");
    assert!(
        seeded.adopted.iter().all(|(node, _)| *node != id("3")),
        "{:?}",
        seeded.adopted
    );
    assert_eq!(state(&s, &id("3")).adopted_by, None);
    assert!(state(&s, &id("3")).gone);
}

/// End to end: a 20,000-row roadmap with a detail heading per row is
/// read, synced and derived, and a re-read writes nothing. Before the fix the parse
/// alone took 202 s in a debug build, so a quadratic regression shows as a test that
/// does not finish. The elapsed time is checked only under `KGRAF_TIME_BUDGET`.
#[test]
fn a_large_roadmap_is_presumed_and_a_re_read_writes_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    let n = 20_000;
    let mut doc = String::from("## P\n\n| Phase | What | Status |\n|---|---|---|\n");
    for i in 0..n {
        doc.push_str(&format!("| {i} | thing {i} | Complete |\n"));
    }
    for i in 0..n {
        doc.push_str(&format!("\n### Phase {i}: thing\n\nIn `src/m{i}.rs`.\n"));
    }
    write(dir.path(), "ROADMAP.md", &doc);
    let s = store(dir.path(), db.path());
    let started = std::time::Instant::now();
    let r = presume(&s, 1);
    let views = presumed::views(&s).expect("views");
    let first = started.elapsed();
    assert_eq!(r.items, n);
    assert_eq!(views.len(), n + 2);
    let started = std::time::Instant::now();
    assert!(!presume(&s, 2).sync.wrote());
    let again = started.elapsed();
    eprintln!("large roadmap: first read {first:?}, re-read {again:?}");
    if time_budget_enforced() {
        let bound = std::time::Duration::from_secs(60);
        assert!(first < bound && again < bound, "{first:?}, {again:?}");
    }
}

#[test]
fn a_document_with_no_structure_yields_no_items_and_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    write(dir.path(), "TODO.md", "# Todo\n\nJust prose, no list.\n");
    let s = store(dir.path(), db.path());
    let r = presumed::presume_roadmaps(&s, &["TODO.md".to_owned()], 1).expect("presume");
    assert_eq!(r[0].items, 0);
    assert_eq!(
        r[0].shape,
        "0 status table(s) with 0 row(s), 0 task item(s), 0 status heading(s)"
    );
}

#[test]
fn a_damaged_node_body_is_shown_as_damaged_and_hides_no_other_node() {
    let (_dir, _db, s) = fixture();
    presume(&s, 100);
    let mut node = s
        .work_nodes(P)
        .expect("nodes")
        .into_iter()
        .find(|n| n.id == id("2"))
        .expect("2");
    node.body = "{not json".into();
    s.put_work(&[node], &[]).expect("edit");

    // The damage is shown on that node's own row, in words; every other node
    // still reads, and a re-read still succeeds.
    let views = presumed::views(&s).expect("one damaged body does not hide the rest");
    let bad = views.iter().find(|v| v.id == id("2")).expect("damaged row");
    let detail = bad.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("damaged")
            && detail.contains(&id("2"))
            && detail.contains("not valid JSON"),
        "{detail}"
    );
    assert_eq!(
        bad.state,
        PresumedState::Unknown,
        "a damaged body is not remaining work"
    );
    let good = views.iter().find(|v| v.id == id("1")).expect("item 1");
    assert!(good.detail.is_none() || !good.detail.as_deref().unwrap_or("").contains("damaged"));
    presume(&s, 300);
}

/// A damaged item is unknown, not remaining work: it does not turn its group into
/// "remaining", and a group of only damaged items is unknown too.
#[test]
fn a_damaged_item_is_not_counted_as_remaining_in_its_group() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    write(
        dir.path(),
        "ROADMAP.md",
        "# Demo\n\n## Phases\n\n| Milestone | Description | Status |\n|---|---|---|\n\
         | 1 | A | Not started |\n| 2 | B | Not started |\n",
    );
    let s = store(dir.path(), db.path());
    index(&s);
    presume(&s, 100);
    let group = format!("{P}/component/phases");
    assert_eq!(state(&s, &group).state, PresumedState::Remaining);

    let damage = |which: &str| {
        let mut node = s
            .work_nodes(P)
            .expect("nodes")
            .into_iter()
            .find(|n| n.id == id(which))
            .expect("item");
        node.body = "{not json".into();
        s.put_work(&[node], &[]).expect("edit");
    };
    damage("1");
    assert_eq!(state(&s, &id("1")).state, PresumedState::Unknown);
    assert_eq!(
        state(&s, &group).state,
        PresumedState::Partial,
        "one damaged item of two is not remaining"
    );
    damage("2");
    assert_eq!(state(&s, &group).state, PresumedState::Unknown);
}
