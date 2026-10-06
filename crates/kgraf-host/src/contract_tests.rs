//! Interface contracts on the live path: a run built by [`Host`] exactly as
//! `kgraf orchestrate` and the desktop shell build one, with only the model
//! in-process (`Host::with_route_provider`, the `in-process-provider` seam). The
//! decomposition writes an interface contract; the orchestrator registers it,
//! runs stub-compile with its canaries and two-sided vacuity for real in the
//! sandbox over the run's integration branch, compares the handshake, and
//! dispatches; a worker challenges the contract through `challenge_contract`
//! mid-run, and the paid review goes through the ledger. No network.
//!
//! MAST failure modes cited (arXiv:2503.13657): FM-3.2 no or incomplete
//! verification (the contract is checked before any worker is spent on it),
//! FM-1.2 disobey role specification (the handshake is compared by the
//! orchestrator), FM-2.5 ignored other agent's input (a worker's challenge is
//! answered with the artifact or the judgment that settled it).

use crate::{Host, RunConfig, RunStart};
use kgraf_core::RunId;
use kgraf_orchestrator::{
    Basis, CHALLENGE_TOOL, CheckStage, ContractEvent, Evidence, Resolution, RunStatus, TraceEvent,
    TraceLog,
};
use kgraf_provider::{CallToken, Content, Provider, ProviderError, Request, Response, Role};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PY_STUB: &str = "class Config:\n    def __init__(self, port):\n        self.port = port\n\n\
def parse_config(text):\n    return Config(0)\n";

const PY_REFERENCE: &str = "class Config:\n    def __init__(self, port):\n        self.port = port\n\n\
def parse_config(text):\n    return Config(int(text.split('=', 1)[1]))\n";

const PY_TEST: &str = "import unittest\nfrom config import parse_config\n\n\
class T(unittest.TestCase):\n    def test_port(self):\n        self.assertEqual(parse_config('port=8080').port, 8080)\n";

const PY_CONSUMER: &str = "from config import parse_config, Config\n";

const RULE: &str = "Ports are configured as `port:N`, never `port=N`.";

const PROD_SAYS: &str = "I will provide `parse_config` taking `str` and returning `Config`";
const PROD_SAYS_2: &str = "I will provide `Config` with a `port` field";
const CONS_SAYS: &str = "I assume you provide `parse_config` taking `str` and returning `Config`";

/// The interface the decomposition writes: the contract of
/// `kgraf-orchestrator/tests/contract_flow.rs`, in the plan's JSON.
fn interface_json(compile: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "id": "config",
        "parties": ["prod", "cons"],
        "symbols": ["parse_config", "Config"],
        "stub": [{"path": "config.py", "content": PY_STUB}],
        "reference": [{"path": "config.py", "content": PY_REFERENCE}],
        "consumer": [{"path": "consumer.py", "content": PY_CONSUMER}],
        "tests": [{"path": "test_contract.py", "content": PY_TEST}],
        "compile": compile,
        "test": ["python3", "-m", "unittest", "-q", "test_contract"],
        // Declared by the model; the orchestrator records no read set it did
        // not observe, so this is dropped.
        "consulted": ["docs/RULES.md"]
    })
}

fn add_file(path: &str, content: &str) -> serde_json::Value {
    let body: String = content.lines().map(|l| format!("+{l}\n")).collect();
    serde_json::json!({
        "patch": format!("*** Begin Patch\n*** Add File: {path}\n{body}*** End Patch")
    })
}

/// What the fake model saw, for the assertions.
#[derive(Default)]
struct Seen {
    /// The decomposition's user message (the contracts call).
    decompose: Vec<String>,
    /// The interface call's user message.
    interfaces: Vec<String>,
    /// Each worker's first request: its brief, and its tool names.
    briefs: Vec<(String, Vec<String>)>,
    /// Tool results the producer's worker read after its challenge.
    challenge_answers: Vec<String>,
    handshakes: usize,
    /// Consumer-test requests (0076): each consumer writes one.
    consumer_tests: usize,
    reviews: usize,
}

/// An in-process model for every role, answering by the system prompt: the
/// decomposition (a plan of two contracts and one interface binding them), the
/// handshake (each party from its own prompt), re-grounding, the challenge
/// review, and the two workers. The producer challenges the contract in its
/// first round, then writes the reference; the consumer writes its file.
struct FakeModel {
    compile: Vec<&'static str>,
    seen: Mutex<Seen>,
}

fn text_of(request: &Request) -> (String, String) {
    let system = request
        .system
        .iter()
        .filter_map(Content::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    let messages = request
        .messages
        .iter()
        .flat_map(|m| m.content.iter().filter_map(Content::as_text))
        .collect::<Vec<_>>()
        .join("\n");
    (system, messages)
}

impl FakeModel {
    /// What the model saw so far, taken out: no lock is held across an await.
    fn snapshot(&self) -> Seen {
        std::mem::take(&mut *self.seen.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn answer(&self, request: &Request) -> (Vec<Content>, bool) {
        let (system, user) = text_of(request);
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        let text = |t: String| (vec![Content::text(t)], false);
        // The interface call answers the same plan JSON: it reads only its
        // `interfaces`, as the contracts call reads only its `contracts`.
        let interface_call = system.contains("interface contracts that bind them");
        if system.contains("Split the user's instruction") || interface_call {
            if interface_call {
                seen.interfaces.push(user.clone());
            } else {
                seen.decompose.push(user.clone());
            }
            let plan = serde_json::json!({
                "contracts": [
                    {"id": "prod", "instruction": "Implement the parser in config.py.",
                     "acceptance": "the contract test passes",
                     "writes": [{"path": "config.py"}], "tools": ["apply_patch"]},
                    {"id": "cons", "instruction": "Use the parser in consumer.py.",
                     "acceptance": "consumer.py imports the parser",
                     "writes": [{"path": "consumer.py"}], "tools": ["apply_patch"]}
                ],
                "interfaces": [interface_json(&self.compile)]
            });
            return text(plan.to_string());
        }
        if system.starts_with("You are a kGraf worker about to consume an interface") {
            seen.consumer_tests += 1;
            // Not JSON: a malformed answer, asked again within the shared
            // bound, after which the writer fails and the failure is recorded
            // (0076). This test is about the checks and the challenge, not
            // the consumer test's content.
            return text("no test".to_owned());
        }
        if system.contains("state your assumptions") {
            seen.handshakes += 1;
            let statement = if user.contains("You are party `prod`") {
                serde_json::json!({"provide": [PROD_SAYS, PROD_SAYS_2], "touch": ["config.py"]})
            } else {
                serde_json::json!({"expect": [CONS_SAYS], "touch": ["consumer.py"]})
            };
            return text(statement.to_string());
        }
        if system.contains("still serves") {
            return text(r#"{"serves":true,"reason":"on track"}"#.into());
        }
        if system.contains("You review an interface contract") {
            seen.reviews += 1;
            return text(
                r#"{"contradicts":false,"contract_excerpt":"","reason":"the rule governs configuration files, not the parser's input"}"#
                    .into(),
            );
        }
        if system.contains("kGraf worker") {
            let round = request
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count();
            let producer = user.contains("Implement the parser");
            if round == 0 {
                seen.briefs.push((
                    user.clone(),
                    request.tools.iter().map(|t| t.name.clone()).collect(),
                ));
            }
            let tool = |name: &str, input: serde_json::Value| {
                (
                    vec![Content::ToolUse {
                        id: format!("t{round}"),
                        name: name.into(),
                        input,
                        signature: None,
                    }],
                    true,
                )
            };
            return match (producer, round) {
                (true, 0) => tool(
                    CHALLENGE_TOOL,
                    serde_json::json!({
                        "interface": "config",
                        "claim": "the contract's test parses `port=8080`, which the project rules forbid",
                        "evidence": {"kind": "quote", "artifact": "docs/RULES.md", "text": RULE}
                    }),
                ),
                (true, 1) => {
                    if let Some(Content::ToolResult { content, .. }) =
                        request.messages.last().and_then(|m| m.content.first())
                    {
                        seen.challenge_answers.push(content.clone());
                    }
                    tool("apply_patch", add_file("config.py", PY_REFERENCE))
                }
                (false, 0) => tool("apply_patch", add_file("consumer.py", PY_CONSUMER)),
                _ => text("done".into()),
            };
        }
        text("unexpected".into())
    }
}

#[async_trait::async_trait]
impl Provider for FakeModel {
    fn adapter(&self) -> &'static str {
        "fake"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let (content, tool_use) = self.answer(request);
        let usage = kgraf_core::TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..Default::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content,
            stop_reason: if tool_use {
                kgraf_provider::StopReason::ToolUse
            } else {
                kgraf_provider::StopReason::EndTurn
            },
            usage,
            request_id: None,
        })
    }
    fn capabilities(&self) -> kgraf_provider::capability::Capabilities<'_> {
        kgraf_provider::capability::Capabilities::none()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A Python project with a recorded rule, committed; the run's configuration
/// on one in-process route.
/// `route` names the in-process route. Each test takes its own so that the
/// process-wide record of the last prefix per route (the cache-miss notice)
/// is not shared between tests running at once; runs themselves each have
/// their own prefix registry.
fn project(route: &str) -> (tempfile::TempDir, PathBuf, RunConfig) {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("docs")).expect("mkdir");
    std::fs::write(root.join("README.md"), "python project\n").expect("write");
    std::fs::write(root.join("docs/RULES.md"), format!("{RULE}\n")).expect("write");
    std::fs::write(root.join(".gitignore"), ".kgraf/\n").expect("write");
    git(&root, &["init", "-q", "-b", "main"]);
    // The owner's identity in the repository itself, never the user's global
    // config: the host refuses to build without one (merge-back commits).
    git(&root, &["config", "user.name", "Test Owner"]);
    git(&root, &["config", "user.email", "owner@example.invalid"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let prices = dir.path().join("prices.json");
    std::fs::write(
        &prices,
        r#"{"m": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
    )
    .expect("prices");
    let mut config = RunConfig::new(&root, vec![format!("{route}:m")], &prices);
    config.verify = "true".into();
    // Contract checks are off by default; these tests turn them on, except
    // the one that tests the default.
    config.contract_checks = true;
    config.check_timeout = Duration::from_secs(120);
    config.check_compile_timeout = Duration::from_secs(120);
    (dir, root, config)
}

fn host(config: RunConfig, route: &str, model: Arc<FakeModel>) -> Host {
    Host::open(config)
        .expect("open")
        .with_route_provider(route, model)
        .expect("in-process route")
}

fn fake(compile: &[&'static str]) -> Arc<FakeModel> {
    Arc::new(FakeModel {
        compile: compile.to_vec(),
        seen: Mutex::new(Seen::default()),
    })
}

const COMPILE: &[&str] = &["python3", "-c", "import consumer, test_contract"];

/// The run's contract events, from its trace. The run's own handle closes the
/// file on a blocking thread after the run returns, so the lock can outlive
/// the run by a moment.
async fn contract_events(root: &Path, run: &str) -> Vec<ContractEvent> {
    let path = root
        .join(".kgraf")
        .join("runs")
        .join(run)
        .join("trace.jsonl");
    let started = Instant::now();
    let events = loop {
        match TraceLog::open(&path).await {
            Ok(log) => break log.load().await.expect("load"),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("trace: {e:?}"),
        }
    };
    events
        .into_iter()
        .filter_map(|e| match e {
            TraceEvent::Contract { record, .. } => Some(*record),
            _ => None,
        })
        .collect()
}

/// The live path end to end: registration, the three checks for real in the
/// sandbox (stub-compile with both canaries, vacuity failing on the stub and
/// passing on the reference), the handshake of both parties, dispatch, a
/// worker's challenge settled by a paid review and answered on judgment, and
/// every orchestrator call of it in the ledger under the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_run_checks_its_interface_contract_and_answers_a_workers_challenge() {
    let (_dir, root, config) = project("fake-live");
    let model = fake(COMPILE);
    let host = host(config, "fake-live", model.clone());
    let prepared = host
        .prepare(RunId::new("live"), None, &mut |_| {})
        .await
        .expect("prepare");
    assert!(prepared.shape().contract_checks);
    let outcome = prepared
        .run(RunStart::Instruction(
            "Add a config parser and a consumer of it.".into(),
        ))
        .await
        .expect("run");
    let report = outcome.report;
    if report.status != RunStatus::Completed {
        let trace =
            std::fs::read_to_string(root.join(".kgraf/runs/live/trace.jsonl")).unwrap_or_default();
        eprintln!("{trace}");
    }
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let mut verified = report.verified.clone();
    verified.sort();
    assert_eq!(verified, vec!["cons".to_owned(), "prod".to_owned()]);

    // The contracts call was told an interface call follows, and the
    // interface call was given the accepted contracts.
    let seen = model.snapshot();
    assert!(
        seen.decompose
            .iter()
            .all(|u| u.contains("Contract checks are on")),
        "the contracts call says interfaces follow"
    );
    assert_eq!(seen.interfaces.len(), 1, "one interface call");
    assert!(
        seen.interfaces[0].contains("Accepted contracts (fixed;")
            && seen.interfaces[0].contains("prod:")
            && seen.interfaces[0].contains("cons:"),
        "{}",
        seen.interfaces[0]
    );

    let events = contract_events(&root, "live").await;
    // 1. Registered, with the model's declared read set dropped.
    let registered = events.iter().find_map(|e| match e {
        ContractEvent::Registered { interface } => Some(interface.clone()),
        _ => None,
    });
    let registered = registered.expect("the interface was registered");
    assert_eq!(registered.label(), "config@v1");
    assert_eq!(
        registered.parties,
        vec!["prod".to_owned(), "cons".to_owned()]
    );
    assert!(
        registered.consulted.is_empty(),
        "a read set is observed, never declared"
    );

    // 2. The mechanical checks ran for real, in order, with the outcome each
    // required; then the handshake of both parties.
    let (runs, handshake) = events
        .iter()
        .find_map(|e| match e {
            ContractEvent::ChecksPassed {
                interface,
                runs,
                handshake,
                ..
            } if interface == "config" => Some((runs.clone(), handshake.clone())),
            _ => None,
        })
        .expect("the checks passed");
    let got: Vec<(CheckStage, &str, bool, bool)> = runs
        .iter()
        .map(|r| (r.stage, r.against.as_str(), r.success, r.expected))
        .collect();
    assert_eq!(
        got,
        vec![
            (CheckStage::StubCompile, "stub", true, true),
            (CheckStage::StubCompile, "canary test", false, true),
            (CheckStage::StubCompile, "canary consumer", false, true),
            (CheckStage::Vacuity, "stub", false, true),
            (CheckStage::Vacuity, "reference", true, true),
        ]
    );
    let mut parties: Vec<&str> = handshake.iter().map(|s| s.party.as_str()).collect();
    parties.sort();
    assert_eq!(parties, vec!["cons", "prod"]);
    assert_eq!(seen.handshakes, 2);
    // The fake's answer is not JSON, so it is malformed: asked once and again
    // up to the shared bound, then the writer fails and the run goes on.
    assert_eq!(
        seen.consumer_tests,
        1 + kgraf_provider::MAX_MALFORMED_OUTPUT_RETRIES as usize,
        "the consumer's one test is asked for within the malformed-answer bound"
    );

    // 3. Each worker's brief carried the interface and its test, and the tool.
    assert_eq!(seen.briefs.len(), 2);
    for (brief, tools) in &seen.briefs {
        assert!(brief.contains("Interface contract `config@v1`"), "{brief}");
        assert!(
            brief.contains("parse_config('port=8080')"),
            "the test is shown"
        );
        assert!(
            brief.contains("the contract test is informational until it is committed")
                && !brief.contains("must pass against your work"),
            "the brief claims no run of the test that does not happen"
        );
        assert!(
            brief.contains("<interface_symbols interface=\"config@v1\">\nparse_config\nConfig\n"),
            "the model-written surface is a data block: {brief}"
        );
        assert!(tools.iter().any(|t| t == CHALLENGE_TOOL), "{tools:?}");
    }

    // 4. The challenge: from the producer's attempt, on the in-process vendor,
    // looked up (the quote is in docs/RULES.md), reviewed once, and rejected on
    // judgment, which is what the worker read back (0037).
    let (challenge, resolution, reviewed) = events
        .iter()
        .find_map(|e| match e {
            ContractEvent::Challenged {
                challenge,
                resolution,
                reviewed,
                ..
            } => Some((challenge.clone(), resolution.clone(), *reviewed)),
            _ => None,
        })
        .expect("the worker's challenge was resolved");
    assert_eq!(challenge.challengers.len(), 1);
    assert_eq!(challenge.challengers[0].contract, "prod");
    assert_eq!(challenge.challengers[0].vendor, "in-process");
    assert!(matches!(challenge.evidence, Evidence::Quote { .. }));
    assert!(reviewed, "a verified quote goes to one paid review");
    assert!(
        matches!(
            &resolution,
            Resolution::Rejected {
                basis: Basis::Judgment,
                ..
            }
        ),
        "{resolution:?}"
    );
    assert_eq!(seen.reviews, 1);
    assert_eq!(seen.challenge_answers.len(), 1);
    assert!(
        seen.challenge_answers[0].starts_with("rejected on judgment"),
        "{}",
        seen.challenge_answers[0]
    );
    assert_eq!(report.contracts.challenges, 1);
    assert_eq!(report.contracts.checks_passed, 1);
    assert_eq!(report.contracts.interfaces_registered, 1);

    // 5. Every orchestrator call of the checks is in the ledger, under the run.
    let calls = host.ledger().calls().expect("calls");
    let of_kind = |kind: &str| {
        calls
            .iter()
            .filter(|c| c.trace.kind.as_deref() == Some(kind))
            .collect::<Vec<_>>()
    };
    let handshakes = of_kind("handshake");
    let reviews = of_kind("challenge_review");
    assert_eq!(handshakes.len(), 2);
    assert_eq!(reviews.len(), 1);
    for c in handshakes.iter().chain(&reviews) {
        assert_eq!(c.agent_id, "orchestrator:live");
        assert_eq!(c.task_id, "live");
        assert_eq!(c.settlement, kgraf_provider::Settlement::Counted);
    }
    let prefix = format!("live-{}-", std::process::id());
    let left: Vec<String> =
        std::fs::read_dir(crate::host::check_scratch_root().expect("scratch root"))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(&prefix))
                    .collect()
            })
            .unwrap_or_default();
    assert!(
        left.is_empty(),
        "the run's check scratch is removed at its end: {left:?}"
    );
}

/// A run's scratch is a private `mkdtemp` directory, gone when the
/// prepared run is dropped unstarted; a dead process's directory is swept at
/// the next startup, a live one's is kept.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_scratch_is_private_removed_on_drop_and_swept_when_its_owner_is_dead() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, _root, config) = project("fake-scratch");
    let host = host(config, "fake-scratch", fake(COMPILE));
    let root = crate::host::check_scratch_root().expect("scratch root");
    let mine = |root: &Path| -> Vec<PathBuf> {
        let prefix = format!("scratch1-{}-", std::process::id());
        std::fs::read_dir(root)
            .map(|d| {
                d.flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    let prepared = host
        .prepare(RunId::new("scratch1"), None, &mut |_| {})
        .await
        .expect("prepare");
    let made = mine(&root);
    assert_eq!(made.len(), 1, "{made:?}");
    let mode = std::fs::metadata(&made[0])
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "{mode:o}");
    drop(prepared);
    assert!(
        mine(&root).is_empty(),
        "removed when the unstarted run is dropped"
    );

    // A dead owner: a child that has exited and been reaped.
    let mut child = std::process::Command::new("true").spawn().expect("spawn");
    let dead = child.id();
    child.wait().expect("wait");
    let dead_dir = root.join(format!("gone-{dead}-abc123"));
    let live_dir = root.join("kept-1-abc123");
    std::fs::create_dir_all(&dead_dir).expect("mkdir");
    std::fs::create_dir_all(&live_dir).expect("mkdir");
    let swept = crate::host::sweep_check_scratch();
    assert!(swept.contains(&dead_dir), "{swept:?}");
    assert!(!dead_dir.exists());
    assert!(live_dir.exists(), "PID 1 is alive: its directory stays");
    std::fs::remove_dir_all(&live_dir).expect("cleanup");
}

/// A contract whose compile program is not installed is a check that could
/// not run, not a stub-compile failure of the contract: its parties are held,
/// no worker is spent on them, and the reason names the program.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_contract_in_a_language_this_machine_cannot_check_holds_its_parties() {
    let (_dir, root, config) = project("fake-nolang");
    let model = fake(&["kgraf-no-such-compiler", "--check"]);
    let host = host(config, "fake-nolang", model.clone());
    let outcome = host
        .prepare(RunId::new("nolang"), None, &mut |_| {})
        .await
        .expect("prepare")
        .run(RunStart::Instruction("Add a config parser.".into()))
        .await
        .expect("run");
    let report = outcome.report;
    assert!(report.verified.is_empty(), "{report:?}");
    let mut held: Vec<&str> = report.held.iter().map(|(c, _)| c.as_str()).collect();
    held.sort();
    assert_eq!(held, vec!["cons", "prod"], "{report:?}");
    assert!(
        report
            .held
            .iter()
            .all(|(_, why)| why.contains("kgraf-no-such-compiler")),
        "{:?}",
        report.held
    );
    let events = contract_events(&root, "nolang").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ContractEvent::CheckNotRun { error, .. } if error.contains("not installed"))),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ContractEvent::CheckFailed { .. })),
        "a missing toolchain is no verdict on the contract"
    );
    let seen = model.snapshot();
    assert!(seen.briefs.is_empty(), "no worker was spent");
    assert_eq!(report.contracts.checks_not_run, 1);
}

/// Turned off, the run asks for no interface, offers no challenge tool, and
/// records no contract event: the switch changes the run, not a field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_contract_checks_no_interface_is_asked_checked_or_offered() {
    let (_dir, root, mut config) = project("fake-off");
    // The default, not a setting written here: off unless asked for.
    config.contract_checks = crate::config::defaults::contract_checks();
    assert!(!config.contract_checks);
    let model = fake(COMPILE);
    let host = host(config, "fake-off", model.clone());
    let prepared = host
        .prepare(RunId::new("off"), None, &mut |_| {})
        .await
        .expect("prepare");
    assert!(
        prepared
            .shape()
            .routes
            .iter()
            .all(|r| !r.tools.iter().any(|t| t == CHALLENGE_TOOL))
    );
    let outcome = prepared
        .run(RunStart::Instruction("Add a config parser.".into()))
        .await
        .expect("run");
    let seen = model.snapshot();
    assert!(
        seen.decompose
            .iter()
            .all(|u| !u.contains("Contract checks are on")),
        "no interface is asked for"
    );
    assert!(seen.interfaces.is_empty(), "no interface call is made");
    assert_eq!(seen.handshakes, 0);
    assert!(
        seen.briefs
            .iter()
            .all(|(b, tools)| !b.contains("Interface contract")
                && !tools.iter().any(|t| t == CHALLENGE_TOOL))
    );
    assert_eq!(
        outcome.report.status,
        RunStatus::Completed,
        "{:?}",
        outcome.report
    );
    assert!(contract_events(&root, "off").await.is_empty());
    assert_eq!(
        outcome.report.contracts,
        kgraf_orchestrator::ContractCounters::default()
    );
}

/// Two runs in one process on the same route with different tool lists
/// (contract checks on, then off) both run, one after the other and at the
/// same time; each run admits its prefix in its own registry. The second
/// build of the sequence says its prefix changed: a cache miss, not an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_with_different_tool_lists_share_a_route_in_one_process() {
    const ROUTE: &str = "fake-shared";
    async fn one(checks: bool, run: &str) -> (RunStatus, Vec<crate::Notice>) {
        let (_dir, _root, mut config) = project(ROUTE);
        config.contract_checks = checks;
        let host = host(config, ROUTE, fake(COMPILE));
        let mut notices = Vec::new();
        let outcome = host
            .prepare(RunId::new(run), None, &mut |n| notices.push(n))
            .await
            .expect("prepare")
            .run(RunStart::Instruction("Add a config parser.".into()))
            .await
            .expect("run");
        (outcome.report.status, notices)
    }
    // Each run's future is boxed: unboxed, the test thread (2 MiB) holds
    // both inline, which a debug build's unoptimized state machines overflow
    // on Linux. An optimized build needs under 512 KiB for the same path.
    let (first, _) = Box::pin(one(true, "seq-on")).await;
    assert_eq!(first, RunStatus::Completed);
    let (second, notices) = Box::pin(one(false, "seq-off")).await;
    assert_eq!(
        second,
        RunStatus::Completed,
        "not refused by the first run's prefix"
    );
    assert!(
        notices.iter().any(|n| matches!(
            n,
            crate::Notice::PrefixChanged { route, .. } if route == &format!("{ROUTE}:m")
        )),
        "{notices:?}"
    );
    let (a, b) = tokio::join!(
        Box::pin(one(true, "con-on")),
        Box::pin(one(false, "con-off"))
    );
    assert_eq!(a.0, RunStatus::Completed);
    assert_eq!(b.0, RunStatus::Completed);
}

/// A scratch root writable by others, or a symbolic link, is passed over
/// for the next candidate; a missing one is made with mode 0700; with no safe
/// candidate the host fails loud, naming each problem.
#[cfg(unix)]
#[test]
fn an_unsafe_scratch_root_is_passed_over_and_none_safe_fails_loud() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("dir");
    let open = dir.path().join("open");
    std::fs::create_dir(&open).expect("mkdir");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(dir.path(), &link).expect("link");
    let fresh = dir.path().join("fresh/checks");

    let chosen =
        crate::host::safe_scratch_root(&[open.clone(), link.clone(), fresh.clone()]).expect("safe");
    assert_eq!(chosen, fresh);
    let mode = std::fs::metadata(&fresh)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "{mode:o}");

    let err = crate::host::safe_scratch_root(&[open, link])
        .expect_err("no safe candidate")
        .to_string();
    assert!(
        err.contains("writable by others (mode 777)") && err.contains("not a directory of its own"),
        "{err}"
    );
}

/// A scratch root below a cargo file another user could
/// have planted (here a `.cargo/config.toml` anyone may write, as in a shared
/// `/tmp`) is passed over: cargo in a check would read it. A root below the
/// owner's own cargo file is kept.
#[cfg(unix)]
#[test]
fn a_scratch_root_below_a_foreign_cargo_file_is_passed_over() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("dir");
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(shared.join(".cargo")).expect("mkdir");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::set_permissions(
        shared.join(".cargo"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    let planted = shared.join(".cargo/config.toml");
    std::fs::write(&planted, "[target.'cfg(all())']\nrunner = \"true\"\n").expect("write");
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o666)).expect("chmod");
    let own = dir.path().join("own");
    std::fs::create_dir(&own).expect("mkdir");
    std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::write(own.join("Cargo.toml"), "[package]\n").expect("write");
    std::fs::set_permissions(
        own.join("Cargo.toml"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod");

    let below_planted = shared.join("checks");
    let below_own = own.join("checks");
    let chosen =
        crate::host::safe_scratch_root(&[below_planted.clone(), below_own.clone()]).expect("safe");
    assert_eq!(chosen, below_own);
    let err = crate::host::safe_scratch_root(&[below_planted])
        .expect_err("only a root below the planted file")
        .to_string();
    assert!(
        err.contains("config.toml") && err.contains("another user"),
        "{err}"
    );
}
