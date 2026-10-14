//! A run with interface checks and no new flag dispatches as it did before
//! planner calls could run as tasks (decision 0100, section 4).
//!
//! The scenario: two parties of one interface (`prod`, `cons`), a contract no
//! interface binds (`free`), and one that depends on it (`after`). The
//! handshake is held while `free`'s worker finishes; then it is released.
//! Before the change, the loop made the handshake itself, so `free`'s finish
//! waited behind it and `after` was dispatched last. With the gate off the
//! loop still waits for the handshake, so the order is the same; with the
//! gate on, `after` would go before the parties.
//!
//! The no-flag test body (with a consumer test and a re-grounding in the
//! run) was run on `main` before planner calls could run as tasks
//! (`8d1c2644`), with nothing else changed, three times alike, and what it
//! recorded is pinned here: the dispatch order and the ledger's calls by agent
//! and kind. With the gate on, `after` goes before the parties, with the same
//! calls.

mod contract_support;
mod support;

use contract_support::{commit_all, py, runner_with, state_dir};
use kgraf_core::RunId;
use kgraf_orchestrator::{
    ContractChecks, FileOverlay, GitStubLander, InterfaceContract, ModelCaller,
    ModelConsumerTestWriter, ModelHandshaker, ModelReviewer, Orchestrator, OrchestratorConfig,
    RunStatus, TraceEvent, TraceLog,
};
use kgraf_provider::{CallToken, Provider, ProviderError, Request, Response};
use kgraf_sandbox::{Identity, MergeBack, WorkspaceManager};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;

/// The dispatch order the baseline traced.
const BASELINE_ORDER: &[&str] = &["free", "prod", "cons", "after"];

/// The ledger calls the baseline made, by `<agent>:<kind>`. Agent is the
/// first `-` or `:` separated word of the agent id.
const BASELINE_CALLS: &[(&str, usize)] = &[
    ("orchestrator:consumer_test", 1),
    ("orchestrator:decompose", 1),
    ("orchestrator:handshake", 2),
    ("orchestrator:interfaces", 2),
    ("orchestrator:reground", 1),
    ("r1:worker_round", 8),
];

const STUB: &str = "class Config:\n    def __init__(self, port):\n        self.port = port\n\n\
def parse_config(text):\n    return Config(0)\n";
const REFERENCE: &str = "class Config:\n    def __init__(self, port):\n        self.port = port\n\n\
def parse_config(text):\n    return Config(int(text.split('=', 1)[1]))\n";
const TEST: &str = "import unittest\nfrom config import parse_config\n\n\
class T(unittest.TestCase):\n    def test_port(self):\n        self.assertEqual(parse_config('port=8080').port, 8080)\n";

fn ic() -> InterfaceContract {
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    InterfaceContract {
        kind: kgraf_orchestrator::InterfaceKind::Behavior,
        id: "config".into(),
        version: 1,
        parties: s(&["prod", "cons"]),
        symbols: s(&["parse_config", "Config"]),
        stub: vec![FileOverlay::new("config.py", STUB)],
        wrong: vec![],
        reference: vec![FileOverlay::new("config.py", REFERENCE)],
        consumer: vec![FileOverlay::new(
            "consumer.py",
            "from config import parse_config, Config\n",
        )],
        tests: vec![FileOverlay::new("test_contract.py", TEST)],
        compile: s(&[py(), "-c", "import consumer, test_contract"]),
        test: s(&[py(), "-m", "unittest", "-q", "test_contract"]),
        consulted: vec![],
        replaces: vec![],
    }
}

/// The handshake model: each party answers from its own prompt, after a
/// permit; it counts the calls it was asked.
struct HeldHandshake {
    asked: AtomicUsize,
    permits: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl Provider for HeldHandshake {
    fn adapter(&self) -> &'static str {
        "fake-plan"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let prompt = request
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(kgraf_provider::Content::as_text)
            .collect::<Vec<_>>()
            .join("\n");
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.permits.acquire().await.expect("permits").forget();
        let text = if prompt.contains("You are party `prod`") {
            serde_json::json!({
                "provide": [
                    "I will provide `parse_config` taking `str` and returning `Config`",
                    "I will provide `Config` with a `port` field"
                ],
                "touch": ["config.py"]
            })
        } else {
            serde_json::json!({
                "expect": ["I assume you provide `parse_config` taking `str` and returning `Config`"],
                "touch": ["consumer.py"]
            })
        }
        .to_string();
        let usage = kgraf_core::TokenUsage {
            uncached_input: 200,
            text_output: 50,
            ..Default::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![kgraf_provider::Content::text(text)],
            stop_reason: kgraf_provider::StopReason::EndTurn,
            usage,
            request_id: None,
        })
    }
}

async fn scenario(cfg: OrchestratorConfig) -> (Vec<String>, BTreeMap<String, usize>) {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path().join("pyrepo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    std::fs::write(repo.join("README.md"), "python project\n").expect("write");
    commit_all(&repo);
    std::process::Command::new("git")
        .current_dir(&repo)
        .args(["branch", "kgraf/r1", "HEAD"])
        .output()
        .expect("git branch");
    let manager = Arc::new(
        WorkspaceManager::open(&repo, &dir.path().join("stub-ws")).expect("workspace manager"),
    );
    let merge_back = Arc::new(
        MergeBack::open(
            manager.clone(),
            "kgraf/r1",
            Identity {
                name: "Test Owner".into(),
                email: "owner@example.invalid".into(),
            },
        )
        .expect("merge-back"),
    );
    let plan = plan_json(&[
        c("prod", "implement the parser", "config.py", &[]),
        c("cons", "use the parser", "consumer.py", &[]),
        c("free", "unrelated work", "free.py", &[]),
        c(
            "after",
            "work after the unrelated work",
            "after.py",
            &["free"],
        ),
    ]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    let held_free = parts.hold_workers("unrelated work");
    let handshake = Arc::new(HeldHandshake {
        asked: AtomicUsize::new(0),
        permits: tokio::sync::Semaphore::new(0),
    });
    let caller = |provider: Arc<dyn Provider>| ModelCaller {
        reasoning: Default::default(),
        provider,
        ledger: parts.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 1_000,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    };
    let runs = parts.dir.join("runs");
    std::fs::create_dir_all(&runs).expect("runs dir");
    let checks = ContractChecks {
        interfaces: vec![ic()],
        runner: Arc::new(runner_with(
            &parts.dir,
            &repo,
            vec![state_dir(&parts.dir), runs],
            Duration::from_secs(60),
        )),
        handshaker: Arc::new(ModelHandshaker(caller(handshake.clone()))),
        reviewer: Arc::new(ModelReviewer(caller(ScriptFake::new(|_| "{}".into())))),
        max_supersessions: 2,
        max_reviews: 3,
        recompute_runs: 2,
        // The consumer's test is written by a model and gated, so its call
        // is in the ledger counts too.
        consumer_test_writer: Some(Arc::new(ModelConsumerTestWriter(caller(ScriptFake::new(
            |_| {
                serde_json::json!({
                    "path": "test_consumer_expects.py",
                    "content": "import unittest\nfrom config import parse_config\n\n\
                class T(unittest.TestCase):\n    def test_port(self):\n        \
                self.assertEqual(parse_config('port=1').port, 1)\n"
                })
                .to_string()
            },
        ))))),
        stub_lander: Arc::new(GitStubLander::new(manager, merge_back)),
    };
    let o = Orchestrator::new(cfg, parts.services(WINDOW))
        .expect("orchestrator")
        .with_contract_checks(checks)
        .expect("contracts");
    let run = tokio::spawn(o.start(RunId::new("r1"), "parse the config".into()));
    let guard = Duration::from_secs(300);
    let started = Instant::now();
    while handshake.asked.load(Ordering::SeqCst) == 0 {
        assert!(!run.is_finished(), "the run ended before its handshake");
        assert!(started.elapsed() < guard, "no handshake");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // `free` finishes while the handshake is held; the loop is given time to
    // take its end in, if it is free to.
    held_free.add_permits(1_000);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    handshake.permits.add_permits(1_000);
    let report = tokio::time::timeout(guard, run)
        .await
        .expect("the run ends")
        .expect("join")
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let trace = TraceLog::open(&parts.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let order = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Dispatch { contract, .. } => Some(contract.clone()),
            _ => None,
        })
        .collect();
    let mut calls: BTreeMap<String, usize> = BTreeMap::new();
    for call in parts.ledger.calls().expect("calls") {
        let agent = call.agent_id.split(&['-', ':'][..]).next().unwrap_or("");
        let key = format!("{agent}:{}", call.trace.kind.unwrap_or_default());
        *calls.entry(key).or_default() += 1;
    }
    (order, calls)
}

fn cfg() -> OrchestratorConfig {
    OrchestratorConfig {
        tick: Duration::from_millis(10),
        step_budget: 6,
        speculate_every: None,
        // A re-grounding every second dispatch, so its calls are pinned too.
        reground_every: 2,
        ..OrchestratorConfig::default()
    }
}

fn baseline_calls() -> BTreeMap<String, usize> {
    BASELINE_CALLS
        .iter()
        .map(|(k, n)| ((*k).to_owned(), *n))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_with_interface_checks_and_no_flag_matches_the_run_before_planner_tasks() {
    let (order, calls) = scenario(cfg()).await;
    println!("GOLDEN order={order:?} calls={calls:?}");
    assert_eq!(order, BASELINE_ORDER, "the dispatch order");
    assert_eq!(
        calls,
        baseline_calls(),
        "the ledger's calls by agent and kind"
    );
}

/// The gate on: `after` is dispatched while the handshake is held, before
/// the parties, and the run makes the same calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_gate_on_the_order_changes_and_the_calls_do_not() {
    let (order, calls) = scenario(OrchestratorConfig {
        concurrent_planner: true,
        ..cfg()
    })
    .await;
    // The parties' order between themselves follows which of their tasks
    // answers first; `after` goes before both.
    assert_eq!(
        &order[..2],
        ["free", "after"],
        "the dispatch order: {order:?}"
    );
    let mut parties = order[2..].to_vec();
    parties.sort();
    assert_eq!(parties, ["cons", "prod"], "{order:?}");
    assert_eq!(
        calls,
        baseline_calls(),
        "the ledger's calls by agent and kind"
    );
}
