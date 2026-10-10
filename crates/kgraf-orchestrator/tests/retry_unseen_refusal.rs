//! A verifier's refusal reaches at least one attempt before the contract is
//! held. The first attempt ends before verification (the model announces the
//! work and ends its turn twice, the second time after it is told to
//! continue); the second attempt writes and its check is refused with a
//! compile error. With the default single retry, no attempt would have seen
//! that error: one more attempt is dispatched, and its brief quotes it. The
//! worker is a real worker loop over a scripted model; the briefs are read
//! from the requests the model receives.

mod support;

use kgraf_core::model_profile::ProfileStore;
use kgraf_core::{Protocol, RunId, TokenUsage};
use kgraf_orchestrator::{
    DirWorkspaces, ModelCaller, ModelDecomposer, ModelGrounder, OpenContract, Orchestrator,
    RunStatus, Services, SystemClock, Verdict, VerifiedCommit, Verifier,
};
use kgraf_provider::{
    CallToken, Content, Provider, ProviderError, Request, Response, Role, StopReason,
};
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A compile error in the contract's own file, as cargo prints it.
const OWN_ERROR: &str = "error[E0252]: the name `io` is defined multiple times\n \
--> src/b.rs:3:15\n";

/// The model: an attempt whose brief carries nothing from an earlier attempt
/// only announces the work, when `announce_first` is set; every other attempt
/// writes the file and then answers.
struct Model {
    announce_first: bool,
    openings: Mutex<Vec<String>>,
}

impl Model {
    fn new(announce_first: bool) -> Arc<Self> {
        Arc::new(Self {
            announce_first,
            openings: Mutex::new(Vec::new()),
        })
    }
}

fn opening_of(request: &Request) -> String {
    request
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .iter()
                .filter_map(Content::as_text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl Provider for Model {
    fn adapter(&self) -> &'static str {
        "fake-announcer"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let opening = opening_of(request);
        {
            let mut openings = self.openings.lock().expect("lock");
            if openings.last() != Some(&opening) {
                openings.push(opening.clone());
            }
        }
        let wrote = request.messages.iter().any(|m| {
            m.role == Role::Assistant
                && m.content
                    .iter()
                    .any(|c| matches!(c, Content::ToolUse { name, .. } if name == "write_file"))
        });
        let fresh = !opening.contains("ended before verification")
            && !opening.contains("The verifier refused");
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        let (content, stop) = if self.announce_first && fresh {
            (
                vec![Content::text("I'll update the file now.")],
                StopReason::EndTurn,
            )
        } else if !wrote {
            (
                vec![Content::ToolUse {
                    id: "w1".into(),
                    name: "write_file".into(),
                    input: json!({"path": "src/b.rs", "content": "use std::io;\nuse std::io;\n"}),
                    signature: None,
                }],
                StopReason::ToolUse,
            )
        } else {
            (vec![Content::text("done")], StopReason::EndTurn)
        };
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content,
            stop_reason: stop,
            usage,
            request_id: None,
        })
    }
}

/// Refuses every attempt with [`OWN_ERROR`], counting them.
#[derive(Default)]
struct Refuses {
    seen: AtomicUsize,
}

#[async_trait::async_trait]
impl Verifier for Refuses {
    async fn verify(
        &self,
        _contract: &OpenContract,
        _workspace: &Path,
        _result: &kgraf_runtime::WorkerResult,
    ) -> Result<Verdict, String> {
        self.seen.fetch_add(1, Ordering::SeqCst);
        Ok(Verdict::Fail {
            reason: OWN_ERROR.to_owned(),
        })
    }

    async fn recheck(&self, _commit: &VerifiedCommit) -> Result<bool, String> {
        Ok(false)
    }
}

/// One run of contract `b` with the default retry budget. Returns the run's
/// blocked reason, the verifications made and the attempts' openings.
async fn run(announce_first: bool) -> (String, usize, Vec<String>) {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("profiles dir");
    let plan = json!({"contracts":[
        {"id":"b","instruction":"write the file src/b.rs","acceptance":"src/b.rs builds",
         "writes":[{"path":"src/b.rs"}],"tools":["write_file","read_file"],"route":"alpha"}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let model = Model::new(announce_first);
    let verifier = Arc::new(Refuses::default());
    let caller = |provider: Arc<ScriptFake>| ModelCaller {
        reasoning: Default::default(),
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
        routes: vec![route_with(
            "alpha",
            "vendor-a",
            ALPHA_MODEL,
            model.clone(),
            p.ledger.clone(),
            WINDOW,
            &[],
        )],
        decomposer: Arc::new(ModelDecomposer(caller(p.planner.clone()))),
        grounder: Arc::new(ModelGrounder(caller(p.grounder.clone()))),
        verifier: verifier.clone(),
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
        .start(RunId::new("r1"), "write the file".into())
        .await
        .expect("run");
    let RunStatus::Blocked { reason } = report.status else {
        panic!("every attempt is refused: {report:?}");
    };
    let openings = model.openings.lock().expect("lock").clone();
    (reason, verifier.seen.load(Ordering::SeqCst), openings)
}

/// An empty finish, then a refused attempt. A third
/// attempt is dispatched with the refusal in its brief, and its own refusal
/// holds the contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_no_attempt_saw_gets_one_more_attempt_that_carries_it() {
    let (reason, verified, openings) = run(true).await;
    assert_eq!(
        verified, 2,
        "attempts 2 and 3 reach the verifier: {openings:?}"
    );
    assert!(reason.contains("b: 3 attempts failed"), "{reason}");
    let last = openings.last().expect("an opening");
    assert!(
        last.contains("The verifier refused the previous attempt"),
        "{last}"
    );
    assert!(last.contains("E0252"), "{last}");
    // The second attempt carried the first one's empty finish, as before.
    assert!(
        openings
            .iter()
            .any(|o| o.contains("ended before verification") && o.contains("told to continue")),
        "{openings:?}"
    );
}

/// The bound: when the second attempt already carried the first one's
/// refusal, its own refusal holds the contract after two attempts, as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_an_attempt_already_saw_holds_after_the_retry_budget() {
    let (reason, verified, openings) = run(false).await;
    assert_eq!(verified, 2, "{openings:?}");
    assert!(reason.contains("b: 2 attempts failed"), "{reason}");
}
