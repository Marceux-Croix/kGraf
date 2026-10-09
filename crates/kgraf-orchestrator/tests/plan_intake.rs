//! Plan intake against recorded planner answers: a model's answer, verbatim
//! (`tests/fixtures/planner-answers/`), through the real orchestrator's
//! contracts call. The planner, grounder and workers are fake providers
//! through the real ledger. No network.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunReport, TraceEvent, TraceLog};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;

/// The route name the long-run answer pins.
const PINNED_ROUTE: &str = "ollama:qwen3.6:35b";

fn recorded(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/planner-answers")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A tool the recorded plans name, so their contracts pass the plan rules.
struct NamedTool(&'static str);

#[async_trait::async_trait]
impl kgraf_tools::Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn definition(&self) -> kgraf_provider::message::ToolDefinition {
        kgraf_provider::message::ToolDefinition {
            name: self.0.into(),
            description: "a tool a recorded plan names".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }
    fn access(
        &self,
        _input: &serde_json::Value,
    ) -> Result<kgraf_tools::Access, kgraf_tools::ToolError> {
        Ok(kgraf_tools::Access::none())
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: &kgraf_tools::ToolContext,
        _permit: &kgraf_tools::CallPermit,
    ) -> Result<kgraf_tools::ToolOutput, kgraf_tools::ToolError> {
        Ok(kgraf_tools::ToolOutput::text("done"))
    }
}

/// An orchestrator whose routes offer the recorded plans' tools, the first
/// route named as the long-run answer pins it.
fn orchestrator(p: &Parts) -> Orchestrator {
    let tools: Vec<Arc<dyn kgraf_tools::Tool>> =
        ["apply_patch", "graph_slice", "shell", "send_message"]
            .into_iter()
            .map(|n| Arc::new(NamedTool(n)) as Arc<dyn kgraf_tools::Tool>)
            .collect();
    let mut services = p.services_with(WINDOW, &tools);
    services.routes[0].name = PINNED_ROUTE.into();
    Orchestrator::new(config(), services).expect("orchestrator")
}

/// Plan, then stop. Returns the report.
async fn plan_then_stop(o: Orchestrator) -> RunReport {
    let h = o.handle();
    let run = tokio::spawn(async move { o.start(RunId::new("r1"), "plan".into()).await });
    assert!(h.status().await.is_some(), "the plan was accepted");
    h.stop().await;
    run.await.expect("join").expect("planned")
}

async fn trace(dir: &Path) -> Vec<TraceEvent> {
    let path = dir.join("runs").join("r1").join("trace.jsonl");
    let started = Instant::now();
    loop {
        match TraceLog::open(&path).await {
            Ok(log) => return log.load().await.expect("load"),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("trace: {e:?}"),
        }
    }
}

fn refusals(events: &[TraceEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::PlanRefused { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

fn accepted_ids(events: &[TraceEvent]) -> Vec<String> {
    events
        .iter()
        .find_map(|e| match e {
            TraceEvent::PlanAccepted { contracts, .. } => {
                Some(contracts.iter().map(|c| c.id.clone()).collect())
            }
            _ => None,
        })
        .expect("a plan was accepted")
}

/// The recorded first answer of a live run leaked a reasoning draft (cut off
/// with `...`) before a literal `</think>`, then gave the plan. Taking the
/// text from the first `{` to the last `}` refused it; the plan after the
/// last `</think>` is taken on the first ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_draft_leaked_before_think_end_does_not_cost_an_ask() {
    let dir = tempfile::tempdir().expect("dir");
    let answer = recorded("d1-ask1-leaked-draft.txt");
    assert!(
        answer.contains("</think>"),
        "the fixture is the leaked draft"
    );
    let p = Parts::open(
        dir.path(),
        move |_| answer.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    plan_then_stop(orchestrator(&p)).await;
    let events = trace(dir.path()).await;
    assert!(refusals(&events).is_empty(), "{:?}", refusals(&events));
    assert_eq!(accepted_ids(&events), ["c1", "c2", "c3", "c4", "c5", "c6"]);
    assert_eq!(p.planner.calls(), 1, "one ask");
}

/// The recorded long-run answer made eight chained contracts that all write
/// `shapes/library.py`, verified by one test file, so no contract could be
/// verified alone (the second was refused because the first's work already
/// passed the test). The plan is refused once with a reason the planner can
/// act on; when the planner repeats it, the contracts are merged into one,
/// said in a notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contracts_sharing_one_file_and_one_test_are_refused_then_merged() {
    let dir = tempfile::tempdir().expect("dir");
    let answer = recorded("long-run-one-file.txt");
    let p = Parts::open(
        dir.path(),
        move |_| answer.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    *p.verifier.plan_scope.lock().expect("lock") = Some(vec!["tests/test_library.py".into()]);
    let report = plan_then_stop(orchestrator(&p)).await;
    let events = trace(dir.path()).await;
    let refused = refusals(&events);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains(
            "contracts c1, c2, c3, c4, c5, c6, c7, c8 all write shapes/library.py and are \
             checked by one test (tests/test_library.py), so none can be verified alone: merge \
             them into one contract, or split the file"
        ),
        "{}",
        refused[0]
    );
    let prompts = p.planner.prompts();
    assert_eq!(prompts.len(), 2, "one ask, one re-ask");
    assert!(
        prompts[1].contains("none can be verified alone"),
        "{}",
        prompts[1]
    );
    assert_eq!(accepted_ids(&events), ["c1"], "merged into the first");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("were merged into `c1`")),
        "{:?}",
        report.notices
    );
}

/// Without a narrowed check to tell, contracts sharing a file are left as the
/// planner wrote them: the rule reads the verifier's scope, never guesses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contracts_sharing_a_file_under_a_whole_check_are_left_alone() {
    let dir = tempfile::tempdir().expect("dir");
    let answer = recorded("long-run-one-file.txt");
    let p = Parts::open(
        dir.path(),
        move |_| answer.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    plan_then_stop(orchestrator(&p)).await;
    let events = trace(dir.path()).await;
    assert!(refusals(&events).is_empty());
    assert_eq!(accepted_ids(&events).len(), 8);
}

/// A greenfield run's second answer (after a refusal for an unwritten
/// workspace member) declared no writes in any of its four contracts, while
/// every instruction creates files. kGraf accepted it: a worker may change only
/// its contract's files, so c1's worker was refused on `Cargo.toml` and the
/// contract was held as a plan-shape error, and every write-based intake rule
/// passed because the plan named no file. The answer is refused with a reason
/// the planner can act on, and the corrected answer is taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contracts_that_declare_no_writes_are_refused_and_asked_again() {
    let dir = tempfile::tempdir().expect("dir");
    let writeless = recorded("greenfield-writeless.txt");
    let mut fixed: serde_json::Value = serde_json::from_str(&writeless).expect("json");
    let files = [
        vec!["Cargo.toml"],
        vec!["wcl-core/Cargo.toml", "wcl-core/src/lib.rs"],
        vec!["wcl/Cargo.toml", "wcl/src/main.rs"],
        vec!["wcl-core/tests/count.rs"],
    ];
    for (c, paths) in fixed["contracts"]
        .as_array_mut()
        .expect("contracts")
        .iter_mut()
        .zip(files)
    {
        c["writes"] = paths
            .iter()
            .map(|p| serde_json::json!({ "path": p }))
            .collect();
    }
    let fixed = fixed.to_string();
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.contains("<plan_refused>") {
                fixed.clone()
            } else {
                writeless.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    plan_then_stop(orchestrator(&p)).await;
    let events = trace(dir.path()).await;
    let refused = refusals(&events);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains("contracts `c1`, `c2`, `c3`, `c4` declare no writes"),
        "{}",
        refused[0]
    );
    let prompts = p.planner.prompts();
    assert_eq!(prompts.len(), 2, "one ask, one re-ask");
    assert!(prompts[1].contains("declare no writes"), "{}", prompts[1]);
    assert_eq!(accepted_ids(&events), ["c1", "c2", "c3", "c4"]);
}

/// A Terminal-Bench run's second contracts answer (canary round 14): c1 writes
/// the package, c2 writes the server script, and `c3-install-verify` installs
/// the package from the server and checks an import, declaring no writes. A
/// contract that only installs and verifies changes no file, so the answer is
/// a correct plan. The rule against writeless contracts refused it ("contract
/// `c3-install-verify` declares no writes"); the third answer then came back
/// malformed and the run failed planning. Taken on the first ask, as written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verify_only_contract_next_to_contracts_that_write_is_accepted() {
    let dir = tempfile::tempdir().expect("dir");
    let answer = recorded("tb-pypi-verify-only.txt");
    let plan: serde_json::Value = serde_json::from_str(&answer).expect("json");
    let writes: Vec<usize> = plan["contracts"]
        .as_array()
        .expect("contracts")
        .iter()
        .map(|c| c["writes"].as_array().expect("writes").len())
        .collect();
    assert_eq!(writes, [2, 1, 0], "the recorded shape");
    let p = Parts::open(
        dir.path(),
        move |_| answer.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    plan_then_stop(orchestrator(&p)).await;
    let events = trace(dir.path()).await;
    assert!(refusals(&events).is_empty(), "{:?}", refusals(&events));
    assert_eq!(p.planner.prompts().len(), 1, "one ask");
    assert_eq!(
        accepted_ids(&events),
        ["c1-package-source", "c2-build-serve", "c3-install-verify"]
    );
}
