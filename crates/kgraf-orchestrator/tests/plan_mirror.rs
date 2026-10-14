//! The plan in the graph, mirror only (decision 0100, section 2): a run's
//! contracts are work nodes in the graph, written in the record's own
//! transaction, equal to the record, rebuilt from it on resume, and never able
//! to fail the run.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::latency::LatencyKind;
use kgraf_orchestrator::work_mirror::{item_id, mirror, plan_id};
use kgraf_orchestrator::{Orchestrator, RunStatus, RunStore, TraceEvent, TraceLog};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

fn three_step_plan() -> String {
    plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
        c("c", "make c", "src/c.rs", &["b"]),
    ])
}

/// A finished run's contracts are items of the plan `run:<id>`, the mirror
/// equals what the record gives, and the record's `plan_root` and `source_plan`
/// keep their meaning: the mirror sets neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_writes_a_mirror_equal_to_its_record() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = three_step_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a, b and c".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let store = RunStore::new(p.graph.clone());
    let record = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(true)
    );
    assert_eq!(record.plan_root, Some(RunId::new("r1")));
    assert_eq!(
        record.source_plan, None,
        "the mirror never sets source_plan"
    );

    {
        let g = p.graph.lock().expect("lock");
        let nodes = g.work_nodes(&plan_id("r1")).expect("nodes");
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            ids,
            ["run:r1", "run:r1/item/a", "run:r1/item/b", "run:r1/item/c"]
        );
        assert_eq!(
            g.work_deps(&plan_id("r1")).expect("deps"),
            [
                (item_id("r1", "b"), item_id("r1", "a")),
                (item_id("r1", "c"), item_id("r1", "b"))
            ]
        );
        // A verified contract keeps its whole spec, and each carries the commit
        // that verified it as a fact.
        assert!(nodes[1].body.contains("make a"), "{}", nodes[1].body);
        for id in ["a", "b", "c"] {
            let h = g
                .fact_history(&item_id("r1", id), "work:verified_commit")
                .expect("history");
            assert_eq!(h.len(), 1, "{id}");
            assert_eq!(h[0].object.len(), 40, "{id}");
        }
        // No row says done: doneness is read from the record.
        assert_eq!(mirror(&record).nodes.len(), 4);
    }

    // The write is one `plan_mirror` timing event per plan change, all kGraf
    // time.
    let trace = TraceLog::open(&dir.path().join("runs/r1/trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let events: Vec<_> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Latency(l) if l.kind == LatencyKind::PlanMirror => Some(l),
            _ => None,
        })
        .collect();
    assert!(!events.is_empty(), "the mirror writes were timed");
    // Every save is measured, the many that changed no row too.
    assert!(
        events.iter().any(|e| e.rows == 0),
        "a save that wrote nothing is measured"
    );
    assert!(
        events.iter().any(|e| e.rows > 0),
        "a save that wrote rows is measured"
    );
    assert!(
        events
            .iter()
            .all(|e| e.sums() && e.model_us == 0 && e.tools_us == 0)
    );
}

/// The record as stored, with no mirror: what a kill between the two writes of
/// an older build, or a build from before the mirror, left.
fn record_only(dir: &std::path::Path, record: &kgraf_orchestrator::RunStateRecord) -> Parts {
    let p = Parts::open(
        dir,
        |_| panic!("no planning here"),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.graph
        .lock()
        .expect("lock")
        .set_meta(
            &format!("orchestrator/run/{}", record.run),
            &record.to_json().expect("json"),
        )
        .expect("meta");
    p
}

/// The record wins: a mirror missing, or one that disagrees, is written again
/// from the record by the repair path, and a second repair changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuild_makes_the_mirror_equal_to_the_record() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = three_step_plan();
    let p1 = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(config(), p1.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a, b and c".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = RunStore::new(p1.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");

    let dir2 = tempfile::tempdir().expect("dir");
    let p2 = record_only(dir2.path(), &record);
    let store = RunStore::new(p2.graph.clone());
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(false)
    );
    let repair = store
        .rebuild_mirror(&RunId::new("r1"))
        .expect("rebuild")
        .expect("a record");
    assert!(
        !repair.matched && repair.rows > 0 && repair.items == 3,
        "{repair:?}"
    );
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(true)
    );
    let again = store
        .rebuild_mirror(&RunId::new("r1"))
        .expect("rebuild")
        .expect("a record");
    assert!(again.matched && again.rows == 0, "{again:?}");

    // Disagreement: an item's body is changed in the graph. The record wins.
    {
        let g = p2.graph.lock().expect("lock");
        let mut item = g
            .work_nodes(&plan_id("r1"))
            .expect("nodes")
            .into_iter()
            .find(|n| n.id == item_id("r1", "b"))
            .expect("item");
        item.body = "{\"id\":\"b\",\"instruction\":\"someone else's\"}".into();
        g.put_work(&[item], &[]).expect("edit");
    }
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(false)
    );
    store.rebuild_mirror(&RunId::new("r1")).expect("rebuild");
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(true)
    );
    assert!(
        store
            .rebuild_mirror(&RunId::new("nope"))
            .expect("rebuild")
            .is_none(),
        "no record, nothing to rebuild"
    );
}

/// A resumed run's mirror equals its record. A run is planned and stopped, its
/// record is carried to a graph that has no mirror of it (a kill before the
/// mirror write, or a build from before the mirror), with every contract held so
/// that nothing the resumed run does changes the plan: only the first save of
/// the new process, which has written no item yet, can give it the mirror.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_run_that_changes_nothing_still_gets_its_mirror() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = three_step_plan();
    let p1 = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p1.verifier
        .require
        .lock()
        .expect("lock")
        .replace("never written".into());
    let first = Orchestrator::new(config(), p1.services(WINDOW)).expect("orchestrator");
    let handle = first.handle();
    let run = tokio::spawn(first.start(RunId::new("r1"), "make a, b and c".into()));
    let store1 = RunStore::new(p1.graph.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while store1
        .load(&RunId::new("r1"))
        .expect("load")
        .is_none_or(|r| !r.planned)
    {
        assert!(std::time::Instant::now() < deadline, "the run did not plan");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    handle.stop().await;
    let _ = run.await.expect("join");
    let mut record = store1
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.open_contracts.len(), 3);
    for c in &mut record.open_contracts {
        c.status = kgraf_orchestrator::ContractStatus::Held {
            reason: "held for the test".into(),
        };
    }
    record.status = RunStatus::Stopped;

    let dir2 = tempfile::tempdir().expect("dir");
    let p2 = record_only(dir2.path(), &record);
    let before = p2
        .graph
        .lock()
        .expect("lock")
        .work_nodes("run:r1")
        .expect("nodes");
    assert!(before.is_empty());
    let result = Orchestrator::new(config(), p2.services(WINDOW))
        .expect("orchestrator")
        .resume(RunId::new("r1"))
        .await;
    println!("resume: {:?}", result.as_ref().map(|r| r.status.clone()));
    let store = RunStore::new(p2.graph.clone());
    let stored = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(stored.verified_contracts.len(), 0, "nothing was verified");
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(true)
    );
    assert_eq!(
        p2.graph
            .lock()
            .expect("lock")
            .work_nodes("run:r1")
            .expect("nodes")
            .len(),
        4
    );
}

/// A mirror that cannot be written is said in a notice and never fails the run:
/// the record is saved every time, the run completes, and a later rebuild is
/// told how to be run. Here the graph already holds a node of another plan at
/// an item's id, so every mirror write is refused whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mirror_that_cannot_be_written_is_a_notice_and_the_run_completes() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = three_step_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.graph
        .lock()
        .expect("lock")
        .put_work(
            &[kgraf_graph::WorkNode {
                id: item_id("r1", "a"),
                plan_id: "someone-elses-plan".into(),
                kind: kgraf_graph::WorkKind::Plan,
                parent_id: None,
                title: "in the way".into(),
                body: "{}".into(),
                created_ms: 1,
            }],
            &[],
        )
        .expect("a node in the way");
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a, b and c".into())
        .await
        .expect("the run is not failed by its mirror");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        report.notices.iter().any(|n| n.contains("plan mirror")
            && n.contains("kgraf mirror rebuild r1")
            && !n.contains("  ")),
        "{:?}",
        report.notices
    );
    let store = RunStore::new(p.graph.clone());
    let record = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.status, RunStatus::Completed);
    assert_eq!(record.verified_contracts.len(), 3, "the record is whole");
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(false)
    );
    assert!(
        p.graph
            .lock()
            .expect("lock")
            .work_nodes("run:r1")
            .expect("nodes")
            .iter()
            .all(|n| n.kind == kgraf_graph::WorkKind::Plan),
        "only the plan root was written before the plan: no half of the items stays"
    );
}

/// The three held-plan guards read `source_plan` and nothing else: the re-plan
/// (`replan`), the dropping of held interface checks (`drop_failed_checks`) and
/// the sizing entry (`plan_entry`). The first two are shown by behavior in
/// `contract_flow.rs` (`a_mirrored_run_still_*`). `plan_entry` runs only for a
/// fresh run, whose `plan_root` is not set yet, so no run can reach it with a
/// `plan_root`; its guard is checked in the source: the condition names
/// `source_plan` and not `plan_root`. This reads the code, not a setting, and
/// exists because behavior cannot reach that guard.
#[test]
fn the_sizing_entry_guard_reads_source_plan_only() {
    let src = include_str!("../src/orchestrator/triage_entry.rs");
    let from = src.find("fn plan_entry").expect("plan_entry");
    let guard = &src[from..from + src[from..].find("let facts").expect("the sizing step")];
    assert!(
        guard.contains("self.record.source_plan.is_some()"),
        "{guard}"
    );
    assert!(!guard.contains("plan_root"), "{guard}");
    for (file, src, start) in [
        (
            "orchestrator.rs",
            include_str!("../src/orchestrator.rs"),
            "async fn replan(&mut self)",
        ),
        (
            "contract_hooks.rs",
            include_str!("../src/orchestrator/contract_hooks.rs"),
            "async fn drop_failed_checks(",
        ),
    ] {
        let from = src.find(start).expect(start);
        let window = &src[from..from + 400];
        let line = window
            .lines()
            .find(|l| l.contains("source_plan"))
            .unwrap_or_else(|| panic!("{file}: the guard reads source_plan: {window}"));
        assert!(!line.contains("plan_root"), "{file}: {line}");
    }
}

fn percentile(values: &[i64], pct: usize) -> i64 {
    let mut s = values.to_vec();
    s.sort_unstable();
    let rank = (s.len() * pct).div_ceil(100).max(1);
    s[rank - 1]
}

/// The cost of the mirror on a 200-contract plan, measured: the first write of
/// every item, then 20 saves that each change one item (the unchanged 199 are
/// skipped by their hash), then 20 saves that change nothing. The bounds are
/// loose enough for a loaded host; the figures are printed and the canary twin
/// `plan-graph-stub` records them from a real run.
#[test]
fn a_200_contract_plan_costs_the_mirror_little() {
    let dir = tempfile::tempdir().expect("dir");
    let graph = std::sync::Arc::new(std::sync::Mutex::new(
        kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph"),
    ));
    let store = RunStore::new(graph.clone());
    let mut record = kgraf_orchestrator::RunStateRecord::new(RunId::new("r1"), "200 files", 1_000);
    for i in 0..200 {
        let deps = if i == 0 {
            String::new()
        } else {
            format!(r#""depends_on":["c{:03}"],"#, i - 1)
        };
        let spec: kgraf_orchestrator::ContractSpec = serde_json::from_str(&format!(
            r#"{{"id":"c{i:03}","instruction":"write file {i}","acceptance":"it exists",{deps}"writes":["src/f{i}.rs"]}}"#
        ))
        .expect("spec");
        record.plan_specs.insert(spec.id.clone(), spec);
    }
    let first = store.save_mirrored(&record).expect("first");
    assert_eq!(first.error, None);
    assert!(first.rows >= 201, "{first:?}");
    let mut changed = Vec::new();
    for rep in 0..20 {
        record
            .plan_specs
            .get_mut("c100")
            .expect("spec")
            .acceptance
            .push_str(&format!(" {rep}"));
        let r = store.save_mirrored(&record).expect("save");
        assert_eq!(r.rows, 1, "one item changed, one row written: {r:?}");
        changed.push(r.micros);
    }
    let mut same = Vec::new();
    let builds = store.mirror_builds();
    for _ in 0..20 {
        // The record moves in time only: nothing of the plan changed.
        record.updated_ms += 1;
        let r = store.save_mirrored(&record).expect("save");
        assert_eq!(r.rows, 0, "{r:?}");
        assert!(
            r.skipped,
            "a save that leaves the plan alone builds no mirror: {r:?}"
        );
        same.push(r.micros);
    }
    assert_eq!(
        store.mirror_builds(),
        builds,
        "no mirror was built or diffed"
    );
    println!(
        "plan_mirror, 200 contracts: first write {} us; one item changed p50 {} us p90 {} us; \
         nothing changed p50 {} us p90 {} us",
        first.micros,
        percentile(&changed, 50),
        percentile(&changed, 90),
        percentile(&same, 50),
        percentile(&same, 90)
    );
    assert!(first.micros < 2_000_000, "{first:?}");
    assert!(percentile(&changed, 90) < 250_000, "{changed:?}");
    assert!(percentile(&same, 90) < 20_000, "{same:?}");
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(true)
    );
}

/// A change applied to a finished run reaches the mirror through the real loop: the
/// added contract has its item, its dependency edge and a `work:revised_by` fact
/// naming the change; the first contract keeps its item and its verified commit; the
/// mirror equals the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_applied_change_is_in_the_mirror_as_a_revision() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("src");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let delta = serde_json::json!({"kind": "delta", "add": [{
        "id": "b", "instruction": "make b WRITE src/b.rs", "acceptance": "src/b.rs exists",
        "writes": [{"path": "src/b.rs"}], "depends_on": ["a"], "tools": ["write_file"],
    }]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |prompt: &str| {
            if prompt.contains("<change>") {
                delta.clone()
            } else {
                plan.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("first run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .reopen(
            RunId::new("r1"),
            kgraf_orchestrator::ChangeInput::change(
                "also make b",
                kgraf_orchestrator::Received::now(),
            ),
        )
        .await
        .expect("reopen");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let store = RunStore::new(p.graph.clone());
    let record = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(
        record.changes[0].applied.as_ref().expect("applied").added,
        ["d1.b".to_owned()]
    );
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(true)
    );
    let g = p.graph.lock().expect("lock");
    let nodes = g.work_nodes("run:r1").expect("nodes");
    let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, ["run:r1", "run:r1/item/a", "run:r1/item/d1.b"]);
    assert_eq!(
        g.work_deps("run:r1").expect("deps"),
        [(item_id("r1", "d1.b"), item_id("r1", "a"))]
    );
    let revised = g
        .fact_history(&item_id("r1", "d1.b"), "work:revised_by")
        .expect("history");
    assert_eq!(revised.len(), 1);
    assert_eq!(revised[0].object, "change:1");
    assert!(
        g.fact_history(&item_id("r1", "a"), "work:revised_by")
            .expect("history")
            .is_empty(),
        "an untouched contract has no revision"
    );
    for id in ["a", "d1.b"] {
        assert_eq!(
            g.fact_history(&item_id("r1", id), "work:verified_commit")
                .expect("history")
                .len(),
            1,
            "{id}"
        );
    }
}

/// The mirror is built and diffed before the graph's lock is taken: while another
/// holder keeps the lock, a save has already done its building and waits only to
/// write.
#[test]
fn the_mirror_is_built_before_the_graph_lock_is_taken() {
    let dir = tempfile::tempdir().expect("dir");
    let graph = std::sync::Arc::new(std::sync::Mutex::new(
        kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph"),
    ));
    let store = RunStore::new(graph.clone());
    let mut record = kgraf_orchestrator::RunStateRecord::new(RunId::new("r1"), "go", 1);
    let spec: kgraf_orchestrator::ContractSpec =
        serde_json::from_str(r#"{"id":"a","instruction":"do a","acceptance":"ok"}"#).expect("spec");
    record.plan_specs.insert("a".into(), spec);
    let held = graph.lock().expect("lock");
    let saver = {
        let store = store.clone();
        std::thread::spawn(move || store.save_mirrored(&record).expect("save"))
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while store.mirror_builds() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the save did not build its mirror while the lock was held by another"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!saver.is_finished(), "it is waiting for the lock to write");
    drop(held);
    let report = saver.join().expect("join");
    assert!(report.rows > 0 && report.error.is_none(), "{report:?}");
}

/// The same finished run, its record carried to a graph with a stale mirror of an
/// older shape: an edited body, an item the record does not give (the older
/// shape's revision suffix) and a wrong commit that began later than the right
/// one.
fn stale_mirror(dir: &std::path::Path, record: &kgraf_orchestrator::RunStateRecord) -> Parts {
    let p = record_only(dir, record);
    let right = kgraf_orchestrator::work_mirror::mirror(record);
    {
        let g = p.graph.lock().expect("lock");
        g.put_plan_mirror(&right).expect("a mirror");
        let mut b = g
            .work_nodes(&plan_id("r1"))
            .expect("nodes")
            .into_iter()
            .find(|n| n.id == item_id("r1", "b"))
            .expect("b");
        b.body = r#"{"id":"b","instruction":"an older shape"}"#.into();
        b.title = "an older title".into();
        let mut old = b.clone();
        old.id = item_id("r1", "b.r1");
        g.put_work(&[b, old], &[]).expect("stale");
        g.assert_fact(
            &kgraf_graph::facts::NewFact {
                subject: &item_id("r1", "a"),
                predicate: "work:verified_commit",
                object: "not-the-commit",
                valid_at: i64::MAX / 2,
                invalid_at: None,
                episode: "old",
            },
            1,
        )
        .expect("wrong fact");
    }
    p
}

async fn finished_record() -> kgraf_orchestrator::RunStateRecord {
    let dir = tempfile::tempdir().expect("dir");
    let plan = three_step_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a, b and c".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
}

/// A rebuild converges: a stale body, an extra item and a wrong fact are
/// repaired, the second rebuild writes nothing, and the extra item is marked
/// removed, never deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuild_converges_on_a_stale_mirror_of_an_older_shape() {
    let record = finished_record().await;
    let dir = tempfile::tempdir().expect("dir");
    let p = stale_mirror(dir.path(), &record);
    let store = RunStore::new(p.graph.clone());
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(false)
    );
    let first = store
        .rebuild_mirror(&RunId::new("r1"))
        .expect("rebuild")
        .expect("record");
    assert!(!first.matched && first.rows >= 3, "{first:?}");
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(true)
    );
    let second = store
        .rebuild_mirror(&RunId::new("r1"))
        .expect("rebuild")
        .expect("record");
    assert!(second.matched && second.rows == 0, "converged: {second:?}");
    let g = p.graph.lock().expect("lock");
    let b = g
        .work_nodes(&plan_id("r1"))
        .expect("nodes")
        .into_iter()
        .find(|n| n.id == item_id("r1", "b"))
        .expect("b");
    assert!(b.body.contains("make b"), "{}", b.body);
    let removed = g
        .fact_history(&item_id("r1", "b.r1"), "work:removed_by")
        .expect("history");
    assert_eq!(removed.len(), 1, "the extra item is marked, not deleted");
    let a_commit: Vec<_> = g
        .fact_history(&item_id("r1", "a"), "work:verified_commit")
        .expect("history")
        .into_iter()
        .filter(|f| f.invalid_at.is_none())
        .collect();
    assert_eq!(a_commit.len(), 1);
    assert_eq!(
        a_commit[0].object.len(),
        40,
        "the record's commit is the open one"
    );
}

/// A resume that produces no save (a completed run is refused) still repairs a
/// stale mirror: the check and the repair come before the refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_of_a_completed_run_repairs_a_stale_mirror() {
    let record = finished_record().await;
    let dir = tempfile::tempdir().expect("dir");
    let p = stale_mirror(dir.path(), &record);
    let store = RunStore::new(p.graph.clone());
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(false)
    );
    let refused = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .resume(RunId::new("r1"))
        .await;
    assert!(
        matches!(
            refused,
            Err(kgraf_orchestrator::OrchestratorError::AlreadyCompleted(_))
        ),
        "{refused:?}"
    );
    assert_eq!(
        store.mirror_matches(&RunId::new("r1")).expect("matches"),
        Some(true)
    );
}
