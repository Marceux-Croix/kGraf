//! Decision 0067: `SessionLine::bind` against a real `OrchestratorHandle`, the
//! way `ContractLine::bind` is exercised (`kgraf-host`'s tests build a real
//! `Orchestrator` for that one). `session_tools.rs`'s own unit tests cannot do
//! this: `OrchestratorHandle` has no public constructor outside a built
//! `Orchestrator`, which needs `kgraf-orchestrator`'s own private `Services`
//! and `RunLoop` types, out of reach from a `kgraf-core`-only unit test.

mod support;

use kgraf_orchestrator::{Orchestrator, SessionLine};
use support::*;

const WINDOW: u64 = 200_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_line_binds_once_to_a_real_orchestrator_and_refuses_a_second_binding() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        std::time::Duration::from_millis(1),
    );
    let orchestrator = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");

    let line = SessionLine::new();
    assert!(!line.is_bound(), "a fresh line is unbound");

    line.bind(orchestrator.handle()).expect("first bind");
    assert!(line.is_bound());

    let err = line
        .bind(orchestrator.handle())
        .expect_err("a second binding is refused: one run's tools answer to one orchestrator");
    assert!(
        err.contains("already bound"),
        "the refusal should name why: {err}"
    );
}
