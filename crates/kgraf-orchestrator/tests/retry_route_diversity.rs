//! Regression for decision 0085: a contract's redispatch after its whole
//! flight failed must not land back on the exact route that just failed it.
//!
//! Before the fix, `try_dispatch` always asked `attempt_route` for attempt
//! index `0` on a fresh flight, no matter how many times the same contract
//! had already failed (`contract.failures`, already tracked for the
//! `max_retries` hold decision, was never read by routing). Attempt `0`
//! never escalates (`attempt_route`'s own early return), so a stalled
//! worker's contract, redispatched from `Planned`, was routed exactly like
//! its first, failed attempt -- seen live in 4 of 18
//! orchestrated runs, where a gate
//! remediation's two allowed attempts always landed on the identical model
//! and repeated the identical failure (see
//! `docs/decisions/0085-retry-route-diversity.md`).
//!
//! Both routes here are `StallFake`: whichever the first dispatch picks
//! stalls (the runtime's own stall detector fires well inside the step
//! budget), forcing a retry. With the fix, the retry's route differs from the first attempt's,
//! so both fakes see exactly one call each; without it, one fake would see
//! both calls and the other none.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    DirWorkspaces, ModelCaller, ModelDecomposer, ModelGrounder, Orchestrator, RoutingPolicy,
    RunStatus, Services, SystemClock,
};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// Builds the two-stalling-route harness and runs it once under `routing`.
/// Returns each route's call count, in dispatch-independent (alpha, beta)
/// order.
async fn run_with_policy(routing: RoutingPolicy) -> (usize, usize) {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("profiles dir");
    // Unpinned: `plan_json`/`c()` never set `"route"`, so `route_base` picks
    // fresh instead of following a user pin (a pin must never be overridden;
    // this test is about the *unpinned* auto-pick path only).
    let plan = plan_json(&[c("a", "loop forever", "src/b.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let alpha = Arc::new(StallFake::default());
    let beta = Arc::new(StallFake::default());
    let caller = |provider: Arc<ScriptFake>| ModelCaller {
        provider,
        ledger: p.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 2_000,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    };
    let services = Services {
        graph: p.graph.clone(),
        ledger: p.ledger.clone(),
        prices: prices(),
        routes: vec![
            route_with(
                "alpha",
                "vendor-a",
                ALPHA_MODEL,
                alpha.clone(),
                p.ledger.clone(),
                WINDOW,
                &[],
            ),
            route_with(
                "beta",
                "vendor-b",
                BETA_MODEL,
                beta.clone(),
                p.ledger.clone(),
                WINDOW,
                &[],
            ),
        ],
        decomposer: Arc::new(ModelDecomposer(caller(p.planner.clone()))),
        grounder: Arc::new(ModelGrounder(caller(p.grounder.clone()))),
        verifier: p.verifier.clone(),
        workspaces: Arc::new(DirWorkspaces {
            base: p.dir.join("worktrees"),
        }),
        clock: Arc::new(SystemClock),
        admission: p.admission.clone(),
        runs_dir: p.dir.join("runs"),
        profiles: Arc::new(Mutex::new(
            kgraf_core::model_profile::ProfileStore::default(),
        )),
        profiles_dir: p.dir.join("profiles"),
    };
    let mut cfg = config();
    cfg.routing = routing;
    let report = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "loop forever".into())
        .await
        .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    (
        alpha.calls.load(Ordering::SeqCst),
        beta.calls.load(Ordering::SeqCst),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_contracts_redispatch_moves_off_the_route_that_just_failed_under_rotation() {
    let (a, b) = run_with_policy(RoutingPolicy::Rotation).await;
    assert!(
        a >= 1 && b >= 1,
        "both routes should see the two attempts: alpha={a} beta={b}"
    );
    assert!(
        a < 2 * b.max(1) && b < 2 * a.max(1),
        "one route took the whole retry budget: alpha={a} beta={b}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_contracts_redispatch_moves_off_the_route_that_just_failed_under_fitness() {
    let (a, b) = run_with_policy(RoutingPolicy::Fitness).await;
    assert!(
        a >= 1 && b >= 1,
        "both routes should see the two attempts: alpha={a} beta={b}"
    );
}
