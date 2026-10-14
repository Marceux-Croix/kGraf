//! A run's plan mirror (`run:<id>`, written by the orchestrator for every run) is
//! not an interpretation of the corpus: it is not a prior plan of one.

mod support;

use kgraf_graph::{WorkKind, WorkNode};
use kgraf_interpret::NoTestRuns;
use serde_json::json;
use support::*;

fn mirror_node(plan: &str) -> WorkNode {
    WorkNode {
        id: plan.to_owned(),
        plan_id: plan.to_owned(),
        kind: WorkKind::Plan,
        parent_id: None,
        title: "a run".into(),
        body: "{}".into(),
        created_ms: 1,
    }
}

fn model() -> FakeModel {
    FakeModel::new(|_| {
        answer(
            vec![item(
                "cli",
                &[],
                json!({"paths": ["src/main.rs"], "symbols": ["main"]}),
                "unstated",
                json!([{"text": "cargo test passes", "check": {"kind": "command", "command": "cargo test"}}]),
            )],
            json!([{"command": "cargo test"}]),
            &[],
        )
    })
}

/// A corpus interpretation in a graph that holds two runs' mirrors behaves as
/// one in a graph that holds none: the same prior plans, no `run:` among them.
#[tokio::test]
async fn run_mirrors_are_not_prior_plans() {
    let dir = tempfile::tempdir().expect("dir");
    let db_with = tempfile::tempdir().expect("db");
    let db_without = tempfile::tempdir().expect("db");
    let with = store(dir.path(), db_with.path());
    let without = store(dir.path(), db_without.path());
    with.put_work(&[mirror_node("run:r1"), mirror_node("run:r2")], &[])
        .expect("mirrors");
    // An earlier interpretation stays a prior plan in both.
    for s in [&with, &without] {
        s.put_work(&[mirror_node("an-interpretation")], &[])
            .expect("plan");
    }
    let a = run(
        &with,
        instruction("build me a todo CLI in Rust"),
        &model(),
        &NoTestRuns,
    )
    .await
    .expect("interpret");
    let b = run(
        &without,
        instruction("build me a todo CLI in Rust"),
        &model(),
        &NoTestRuns,
    )
    .await
    .expect("interpret");
    assert_eq!(a.prior_plans(), ["an-interpretation".to_owned()]);
    assert_eq!(a.prior_plans(), b.prior_plans());
}
