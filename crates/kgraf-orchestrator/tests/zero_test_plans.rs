//! Decision 0100's zero-test rule beyond small requests (an opt-in setting)
//! and the background root gate of a reopened small run.
//!
//! - With `zero_test_plans` on, any pass of the root gate (not only a
//!   background run) clears the marks of contracts whose narrowed check
//!   selected no test; with it off, a full plan's marks stay as before.
//! - A small run reopened with added contracts keeps its background root
//!   gate: a failing run while a covered contract is open is information
//!   (no remediation), and the run completes after a pass on the last tip.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::triage::{PlanMode, SmallRequestConfig};
use kgraf_orchestrator::{
    ChangeInput, GateRequest, GateRunner, GateVerdict, Orchestrator, OrchestratorConfig, Received,
    RunStatus, RunStore, UntrustedText,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;
const MARKED: &str =
    "cargo test -p a [scoped_check_vacuous: package a has no recognized test target]";

async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .expect("finished within 60 s")
}

/// A gate runner with a scripted verdict per run and a short delay.
struct Gate {
    script: Vec<bool>,
    runs: AtomicUsize,
    done: AtomicUsize,
}

impl Gate {
    fn new(script: &[bool]) -> Arc<Self> {
        Arc::new(Self {
            script: script.to_vec(),
            runs: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl GateRunner for Gate {
    fn commands(&self) -> Vec<String> {
        vec!["cargo test --workspace".into()]
    }

    async fn run(&self, _request: GateRequest) -> Result<GateVerdict, String> {
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let passed = self.script[n.min(self.script.len() - 1)];
        self.done.fetch_add(1, Ordering::SeqCst);
        Ok(GateVerdict {
            passed,
            by: "cargo test --workspace".into(),
            output: UntrustedText::new(if passed { "ok" } else { "1 test failed" }),
        })
    }
}

fn with_src(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("src")).expect("src");
}

/// A full plan of two contracts whose checks select no test, gated by a
/// root gate that passes (not a background gate: a full plan's gates are
/// as before).
async fn full_plan(zero_test_plans: bool) -> kgraf_orchestrator::RunReport {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    *p.verifier.by.lock().expect("lock") = Some(MARKED.into());
    let mut services = p.services(WINDOW);
    services.workspaces = FakeIntegration::new(dir.path().join("worktrees"));
    let config = OrchestratorConfig {
        zero_test_plans,
        ..config()
    };
    bounded(
        Orchestrator::new(config, services)
            .expect("orchestrator")
            .with_gate_runner(Gate::new(&[true]))
            .start(RunId::new("r1"), "make a and b".into()),
    )
    .await
    .expect("run")
}

/// With the rule on for every contract, the root gate's pass clears the
/// marks; with it off, the same run keeps them (the background-only
/// clearing of a small request applies to no full plan).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_gate_pass_clears_zero_test_marks_only_with_the_setting_on() {
    let on = full_plan(true).await;
    assert_eq!(on.status, RunStatus::Completed, "{on:?}");
    assert!(on.unverified.is_empty(), "{:?}", on.unverified);
    assert!(
        on.notices
            .iter()
            .any(|n| n.contains("no longer unverified")),
        "{:?}",
        on.notices
    );
    let off = full_plan(false).await;
    assert_eq!(off.unverified.len(), 2, "{:?}", off.unverified);
}

/// A small run reopened with two added contracts, the second depending on
/// the first: the background gate's failing run while the second is open is
/// information only (no remediation), and the run completes after the pass
/// on the last tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_small_run_keeps_background_failures_as_information_while_covers_are_open() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let delta = serde_json::json!({
        "kind": "delta",
        "add": [
            {"id": "x", "instruction": "make x WRITE src/x.rs", "acceptance": "src/x.rs exists",
             "writes": [{"path": "src/x.rs"}], "tools": ["write_file"]},
            {"id": "y", "instruction": "make y WRITE src/y.rs", "acceptance": "src/y.rs exists",
             "writes": [{"path": "src/y.rs"}], "depends_on": ["x"], "tools": ["write_file"]},
        ],
    })
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.contains("<change>") {
                delta.clone()
            } else {
                "not a plan".into()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let small = |gate: Arc<Gate>| {
        let mut services = p.services(WINDOW);
        services.workspaces = FakeIntegration::new(dir.path().join("worktrees"));
        let config = OrchestratorConfig {
            small: SmallRequestConfig {
                plan: PlanMode::Auto,
                early_start: false,
            },
            ..config()
        };
        Orchestrator::new(config, services)
            .expect("orchestrator")
            .with_gate_runner(gate)
    };
    let first = bounded(
        small(Gate::new(&[true])).start(RunId::new("r1"), "Fix the greeting WRITE src/a.rs".into()),
    )
    .await
    .expect("small run");
    assert_eq!(first.status, RunStatus::Completed, "{first:?}");
    // After the reopen: y's worker is held until the run on x's tip has
    // ended, so that run (a failure) ends while y is open; then the run on
    // y's tip passes.
    let gate = Gate::new(&[false, true]);
    let hold = p.hold_workers("make y");
    let run = tokio::spawn(small(gate.clone()).reopen(
        RunId::new("r1"),
        ChangeInput::change("add x, then y on it", Received::now()),
    ));
    until("the gate ran on x's tip", || {
        gate.done.load(Ordering::SeqCst) >= 1
    })
    .await;
    hold.add_permits(16);
    let report = bounded(run).await.expect("join").expect("reopen");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert!(record.is_verified("d1.x") && record.is_verified("d1.y"));
    let root = record
        .coordination
        .gates
        .iter()
        .find(|g| g.id == "gate:root")
        .expect("root gate");
    assert!(
        root.remediations.is_empty(),
        "no remediation: {:?}",
        root.remediations
    );
    assert!(root.covers.contains(&"d1.y".to_owned()));
    assert!(
        record
            .coordination
            .background_gates
            .contains_key("gate:root"),
        "still a background gate"
    );
    assert!(
        gate.runs.load(Ordering::SeqCst) >= 2,
        "a run on x's tip and one after"
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("information only while") && n.contains("d1.y")),
        "{:?}",
        report.notices
    );
}
