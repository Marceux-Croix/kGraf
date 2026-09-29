//! End-to-end coverage for decision 0075's live routing tallies: the one write
//! path in `Orchestrator::record_live_outcome`, exercised through a real run
//! rather than by calling `routing::record_live` directly (already covered in
//! `kgraf-orchestrator/src/routing.rs`'s unit tests). `crates/kgraf-host/src/
//! host.rs` gives `Services.profiles_dir` its process state directory, which
//! already exists before a run starts; these tests do the same for the test
//! harness's directory, since `ProfileStore::save` does not create it.

mod support;

use kgraf_core::model_profile::{ProfileStore, Source, TaskKind};
use kgraf_core::{Protocol, RunId, TokenUsage};
use kgraf_orchestrator::{
    DirWorkspaces, ModelCaller, ModelDecomposer, ModelGrounder, Orchestrator, RunStatus, Services,
    SystemClock,
};
use kgraf_provider::{CallToken, Content, Provider, ProviderError, Request, Response, StopReason};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A worker model that never answers: every round issues the identical
/// `write_file` call, which the runtime's stall detector (three repeats,
/// `kgraf-runtime/src/worker.rs`) turns into `WorkerOutcome::Stalled` well
/// inside `config()`'s six-round budget. This is the harness's route to
/// `on_finished`'s live-recording branch (BudgetExhausted or Stalled); the
/// `WorkerFake` in `support` always answers by round 1 and can only reach
/// `on_verified`'s branch.
#[derive(Default)]
struct StallFake {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for StallFake {
    fn adapter(&self) -> &'static str {
        "fake-stall"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            model: request.model.clone(),
            content: vec![Content::ToolUse {
                id: "t1".into(),
                name: "write_file".into(),
                input: json!({"path": "src/b.rs", "content": "same"}),
            }],
            stop_reason: StopReason::ToolUse,
            usage,
            request_id: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verified_pass_leaves_a_live_tally_of_one_attempt_one_satisfied() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("profiles dir");
    let plan = json!({"contracts":[
        {"id":"a","instruction":"make a WRITE src/a.rs","acceptance":"src/a.rs exists",
         "writes":[{"path":"src/a.rs"}],"tools":["write_file"],"route":"alpha"}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let store = ProfileStore::load(&dir.path().join("profiles")).expect("load");
    let profile = store
        .get(ALPHA_MODEL)
        .expect("a live profile for the pinned route's model");
    let tally = profile
        .tally(Source::Live, TaskKind::Implementation)
        .expect("a Live/Implementation tally");
    assert_eq!(tally.attempts, 1, "{tally:?}");
    assert_eq!(tally.satisfied, 1, "{tally:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_attempt_leaves_a_live_tally_with_nothing_satisfied() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("profiles dir");
    let plan = json!({"contracts":[
        {"id":"b","instruction":"loop forever WRITE src/b.rs","acceptance":"src/b.rs exists",
         "writes":[{"path":"src/b.rs"}],"tools":["write_file"],"route":"alpha"}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let stall = Arc::new(StallFake::default());
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
                stall.clone(),
                p.ledger.clone(),
                WINDOW,
                &[],
            ),
            route_with(
                "beta",
                "vendor-b",
                BETA_MODEL,
                p.beta.clone(),
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
        profiles: Arc::new(Mutex::new(ProfileStore::default())),
        profiles_dir: p.dir.join("profiles"),
    };
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "loop forever".into())
        .await
        .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    assert!(
        stall.calls.load(Ordering::SeqCst) >= 3,
        "the stall detector ran"
    );

    let store = ProfileStore::load(&dir.path().join("profiles")).expect("load");
    let profile = store
        .get(ALPHA_MODEL)
        .expect("a live profile for the pinned route's model");
    let tally = profile
        .tally(Source::Live, TaskKind::Implementation)
        .expect("a Live/Implementation tally");
    assert!(tally.attempts >= 1, "{tally:?}");
    assert_eq!(tally.satisfied, 0, "{tally:?}");
}

/// A verifier that cannot run: its answer is an error, never a verdict.
struct BrokenVerifier;

#[async_trait::async_trait]
impl kgraf_orchestrator::Verifier for BrokenVerifier {
    async fn verify(
        &self,
        _contract: &kgraf_orchestrator::OpenContract,
        _workspace: &std::path::Path,
        _result: &kgraf_runtime::WorkerResult,
    ) -> Result<kgraf_orchestrator::Verdict, String> {
        Err("the test runner is not installed".into())
    }

    async fn recheck(&self, _commit: &kgraf_orchestrator::VerifiedCommit) -> Result<bool, String> {
        Ok(true)
    }
}

/// The live tally of `ALPHA_MODEL` after one contract pinned to alpha ran with
/// `verifier` (or the harness's own when `None`): `(attempts, satisfied)`, or
/// `None` when no tally exists.
async fn alpha_tally(
    instruction: &str,
    verifier: Option<Arc<dyn kgraf_orchestrator::Verifier>>,
    require: Option<&str>,
) -> Option<(u64, u64)> {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("profiles dir");
    let plan = json!({"contracts":[
        {"id":"a","instruction":format!("{instruction} WRITE src/a.rs"),
         "acceptance":"src/a.rs exists","writes":[{"path":"src/a.rs"}],
         "tools":["write_file"],"route":"alpha"}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    if let Some(r) = require {
        *p.verifier.require.lock().expect("lock") = Some(r.to_owned());
    }
    let mut services = p.services(WINDOW);
    if let Some(v) = verifier {
        services.verifier = v;
    }
    let mut cfg = config();
    cfg.max_retries = 0;
    Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    let store = ProfileStore::load(&dir.path().join("profiles")).expect("load");
    store
        .get(ALPHA_MODEL)
        .and_then(|p| p.tally(Source::Live, TaskKind::Implementation))
        .map(|t| (t.attempts, t.satisfied))
}

/// A verifier that could not run, or panicked, says nothing about the model:
/// no live attempt is recorded for its route. A verdict that judged the work
/// wrong is recorded, so the absence above is not a harness that records
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_infrastructure_failure_is_not_a_failed_attempt_in_the_live_tally() {
    assert_eq!(
        alpha_tally("make a", Some(Arc::new(BrokenVerifier)), None).await,
        None,
        "a verifier that could not run left a tally"
    );
    assert_eq!(
        alpha_tally("make a PANIC_VERIFY", None, None).await,
        None,
        "a verifier that panicked left a tally"
    );
    assert_eq!(
        alpha_tally("make a", None, Some("text the worker never writes")).await,
        Some((1, 0)),
        "a verdict that judged the work wrong is one failed attempt"
    );
}
