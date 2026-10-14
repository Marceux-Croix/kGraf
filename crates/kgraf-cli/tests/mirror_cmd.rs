//! `kgraf mirror rebuild RUN`: write a run's plan mirror in the graph again from its
//! record. The binary is run for real against a project that holds a record and no
//! mirror (what a kill between the two writes, or an older build, leaves).

use kgraf_core::RunId;
use kgraf_graph::{GraphStore, Indexer};
use kgraf_orchestrator::{ContractSpec, RunStateRecord};
use std::path::Path;
use std::process::{Command, Output};

fn kgraf(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .env("KGRAF_DIAG_DISABLE", "1")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run kgraf")
}

fn project_with_a_record_and_no_mirror() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn foo() {}\n").expect("write");
    std::fs::create_dir_all(dir.path().join(".kgraf")).expect("state");
    let store = GraphStore::open(&dir.path().join(".kgraf/graph.db"), dir.path()).expect("open");
    Indexer::new(&store).index().expect("index");
    let mut record = RunStateRecord::new(RunId::new("r1"), "make a and b", 1);
    for (id, deps) in [("a", "[]"), ("b", r#"["a"]"#)] {
        let spec: ContractSpec = serde_json::from_str(&format!(
            r#"{{"id":"{id}","instruction":"do {id}","acceptance":"ok","depends_on":{deps}}}"#
        ))
        .expect("spec");
        record.plan_specs.insert(id.into(), spec);
    }
    // The record alone: `set_meta` is the write with no mirror in it.
    store
        .set_meta("orchestrator/run/r1", &record.to_json().expect("json"))
        .expect("record");
    dir
}

#[test]
fn rebuild_writes_the_mirror_from_the_record_and_a_second_run_writes_nothing() {
    let dir = project_with_a_record_and_no_mirror();
    let out = kgraf(dir.path(), &["mirror", "rebuild", "r1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("rewritten from the record (2 items,"),
        "{text}"
    );
    {
        let store =
            GraphStore::open_existing_for_facts(&dir.path().join(".kgraf/graph.db"), dir.path())
                .expect("open");
        let nodes = store.work_nodes("run:r1").expect("nodes");
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["run:r1", "run:r1/item/a", "run:r1/item/b"]);
        assert_eq!(
            store.work_deps("run:r1").expect("deps"),
            [("run:r1/item/b".to_owned(), "run:r1/item/a".to_owned())]
        );
    }
    let again = kgraf(dir.path(), &["mirror", "rebuild", "r1"]);
    let text = String::from_utf8_lossy(&again.stdout);
    assert!(again.status.success(), "{text}");
    assert!(text.contains("already matched"), "{text}");
}

#[test]
fn rebuild_of_a_run_with_no_record_fails_and_says_so() {
    let dir = project_with_a_record_and_no_mirror();
    let out = kgraf(dir.path(), &["mirror", "rebuild", "nope"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("there is no run nope"), "{stderr}");
}
