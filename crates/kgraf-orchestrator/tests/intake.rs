//! A run started from a corpus, against fake providers (no network).
//!
//! The interpretation call goes through the ledger and the run's budget like any
//! orchestrator call; the approved plan is seeded into the graph and becomes the
//! decomposition with no decomposition call; work that derives complete is not
//! dispatched again. Each claim is driven, not read back.

mod support;

use kgraf_core::{ModelPrice, Protocol};
use kgraf_core::{Money, RunId};
use kgraf_graph::WorkKind;
use kgraf_interpret::{InterpretRequest, Mode, NoTestRuns, RecordedTests};
use kgraf_orchestrator::{
    IntakeParts, Limits, ModelAsker, ModelCaller, Orchestrator, RunStatus, RunStore, SystemClock,
    interpret_corpus,
};
use kgraf_provider::{CallToken, Provider, ProviderError, Request, Response, Settlement};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

fn write(root: &std::path::Path, path: &str, text: &str) {
    let p = root.join(path);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
    std::fs::write(p, text).expect("write");
}

fn corpus(root: &std::path::Path) {
    write(
        root,
        "ROADMAP.md",
        "# Roadmap\n\n## Parser\nComplete: `parse_config`.\n\n## Exporter\nNot started.\n\n\
         ## Report\nAfter the parser and the exporter.\n",
    );
    write(
        root,
        "src/parser.rs",
        "pub fn parse_config() -> u32 { 1 }\n\n#[test]\nfn parses_a_config() { assert_eq!(parse_config(), 1); }\n",
    );
}

fn interpretation_answer() -> String {
    let item = |id: &str, deps: &[&str], evidence: serde_json::Value, claim: &str| {
        json!({
            "id": id,
            "title": id,
            "instruction": format!("make {id} WRITE src/{id}.rs"),
            "depends_on": deps,
            "acceptance": [{"text": format!("{id} exists"), "check": {"kind": "path", "path": format!("src/{id}.rs")}}],
            "evidence": evidence,
            "writes": [format!("src/{id}.rs")],
            "documents_claim": claim,
        })
    };
    json!({
        "reading": "A config tool: a parser (done), an exporter and a report.",
        "components": [],
        "work_items": [
            item("parser", &[], json!({"paths": ["src/parser.rs"], "symbols": ["parse_config"], "tests": ["parses_a_config"]}), "complete"),
            item("exporter", &[], json!({"symbols": ["export_csv"]}), "not_started"),
            item("report", &["parser", "exporter"], json!({"symbols": ["build_report"]}), "not_started"),
        ],
    })
    .to_string()
}

/// The planner answers the interpretation question; any other question (a
/// decomposition) gets an empty plan, which would fail the run.
fn parts(dir: &std::path::Path) -> Parts {
    Parts::open(
        dir,
        |prompt| {
            if prompt.contains("# Corpus") {
                interpretation_answer()
            } else {
                "{\"contracts\":[]}".into()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    )
}

fn asker_with(provider: Arc<dyn Provider>, p: &Parts, max_output_tokens: u32) -> ModelAsker {
    ModelAsker {
        caller: ModelCaller {
            provider,
            ledger: p.ledger.clone(),
            model: PLAN_MODEL.into(),
            max_output_tokens,
            agent: "orchestrator:r1".into(),
            task: "r1".into(),
        },
        route: "plan".into(),
    }
}

fn asker(p: &Parts) -> ModelAsker {
    asker_with(p.planner.clone(), p, 2_000)
}

/// A request for the asker `asker(&p)` builds.
fn request() -> InterpretRequest {
    InterpretRequest::new(format!("plan:{PLAN_MODEL}"))
}

/// A provider whose call fails after its ledger row is opened.
struct Failing;

#[async_trait::async_trait]
impl Provider for Failing {
    fn adapter(&self) -> &'static str {
        "fake-failing"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        _token: &CallToken,
    ) -> Result<Response, ProviderError> {
        Err(ProviderError::ModelSubstituted {
            requested: request.model.clone(),
            returned: "someone-else".into(),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_starts_from_an_approved_corpus_plan_and_skips_finished_work() {
    let dir = tempfile::tempdir().expect("dir");
    corpus(dir.path());
    let p = parts(dir.path());
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let run = RunId::new("r1");
    let passed = RecordedTests([("parses_a_config".to_owned(), true)].into());

    let mut r = request();
    r.instruction = Some("finish the tool".into());
    let reading = o
        .interpret(&run, r, &asker(&p), &passed)
        .await
        .expect("interpret");
    assert_eq!(reading.mode(), Mode::Resume);
    assert_eq!(p.planner.calls(), 1, "one interpretation call");
    let id = reading.plan_id().to_owned();
    let approved = reading.approve(&id, &[]).expect("approve");
    assert_eq!(approved.skipped(), ["parser".to_owned()]);
    let plan_id = approved.plan_id().to_owned();

    let report = o.start_from_plan(run.clone(), approved).await.expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let mut verified = report.verified.clone();
    verified.sort();
    assert_eq!(verified, ["exporter".to_owned(), "report".to_owned()]);
    assert_eq!(
        p.planner.calls(),
        1,
        "the approved plan is the decomposition: no decomposition call"
    );
    assert!(
        !p.stats
            .requests()
            .iter()
            .any(|r| r.1.contains("make parser")),
        "finished work is not dispatched again"
    );
    // The report ran after the exporter; its dependency on the finished parser was
    // already met.
    let reqs = p.stats.requests();
    let first = |s: &str| reqs.iter().position(|r| r.1.contains(s)).expect(s);
    assert!(first("make exporter") < first("make report"));

    // The plan is in the graph as work nodes.
    let nodes = p
        .graph
        .lock()
        .expect("lock")
        .work_nodes(&plan_id)
        .expect("nodes");
    assert_eq!(nodes.iter().filter(|n| n.kind == WorkKind::Item).count(), 3);
    assert_eq!(nodes[0].kind, WorkKind::Plan);
    // The run re-grounds on the user's instruction, verbatim.
    let record = RunStore::new(p.graph.clone())
        .load(&run)
        .expect("load")
        .expect("record");
    assert_eq!(record.original_instruction, "finish the tool");
    // The record names the plan it runs, so its contracts map to the plan's item
    // nodes (and to the presumed nodes those adopted, decision 0064).
    assert_eq!(record.source_plan.as_deref(), Some(plan_id.as_str()));

    // The interpretation call was charged once: the run's spend equals the ledger
    // cost of every call it made, the interpretation included.
    let prices = prices();
    let ledger_cost = p
        .ledger
        .calls()
        .expect("calls")
        .iter()
        .map(|c| prices.cost(&c.model, &c.usage).expect("priced"))
        .fold(Money::ZERO, |a, b| a + b);
    assert_eq!(report.spent, ledger_cost);
}

/// Decision 0080: kGraf does not limit spend. An interpretation call that would
/// have exceeded any of the old per-task, per-run or per-hour caps still runs to
/// completion, and its cost still lands in the ledger and the run's accounted
/// spend. A stored reading of the same corpus still costs nothing (unrelated to
/// spend limits: it is simply not asked again).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_that_would_have_exceeded_every_old_cap_still_runs_and_is_ledgered() {
    let dir = tempfile::tempdir().expect("dir");
    corpus(dir.path());
    let mut p = parts(dir.path());
    // A run of one orchestrator call's own admission, spending far more than the
    // smallest cap this crate ever offered: nothing here refuses it.
    p.admission = admission(Limits::default());
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    o.interpret(&RunId::new("r1"), request(), &asker(&p), &NoTestRuns)
        .await
        .expect("no cap refuses it");
    assert_eq!(p.planner.calls(), 1, "the call was made and ledgered");
    let prices = prices();
    let ledger_cost = p
        .ledger
        .calls()
        .expect("calls")
        .iter()
        .map(|c| prices.cost(&c.model, &c.usage).expect("priced"))
        .fold(Money::ZERO, |a, b| a + b);
    assert!(
        ledger_cost > Money::ZERO,
        "the call's cost is in the ledger"
    );

    // The same corpus again is answered from the graph with no call and no
    // reservation.
    let dir2 = tempfile::tempdir().expect("dir");
    corpus(dir2.path());
    let p2 = parts(dir2.path());
    let o2 = Orchestrator::new(config(), p2.services(WINDOW)).expect("orchestrator");
    o2.interpret(&RunId::new("r1"), request(), &asker(&p2), &NoTestRuns)
        .await
        .expect("interpret");
    assert_eq!(p2.planner.calls(), 1);
    drop(o2);
    let mut p3 = parts(dir2.path());
    p3.graph = p2.graph.clone();
    p3.admission = p.admission.clone();
    let o3 = Orchestrator::new(config(), p3.services(WINDOW)).expect("orchestrator");
    let again = o3
        .interpret(&RunId::new("r1"), request(), &asker(&p3), &NoTestRuns)
        .await
        .expect("stored");
    assert!(again.is_cached());
    assert_eq!(p3.planner.calls(), 0);
}

fn intake_parts<'a>(
    p: &'a Parts,
    prices: &'a kgraf_core::PriceTable,
    clock: &'a SystemClock,
) -> IntakeParts<'a> {
    IntakeParts {
        graph: &p.graph,
        ledger: &p.ledger,
        prices,
        admission: &p.admission,
        clock,
    }
}

/// A call that fails after its ledger row opened keeps its call id,
/// is settled at its ledger cost (a failed row is free, anything else costs its
/// hold), is returned as charged, and leaves no hold behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_interpretation_call_is_settled_and_reported_as_charged() {
    let dir = tempfile::tempdir().expect("dir");
    corpus(dir.path());
    let p = parts(dir.path());
    let prices = prices();
    let clock = SystemClock;
    let asker = asker_with(Arc::new(Failing), &p, 2_000);
    let intake = interpret_corpus(
        intake_parts(&p, &prices, &clock),
        &RunId::new("r1"),
        request(),
        &asker,
        &NoTestRuns,
    )
    .await;
    assert!(intake.result.is_err());
    assert_eq!(intake.charged.len(), 1, "the failed call's row is kept");
    let row = p
        .ledger
        .call(&intake.charged[0])
        .expect("ledger")
        .expect("row");
    let spent = p.admission.budget().spent_run("r1");
    if row.settlement == Settlement::Failed {
        assert_eq!(spent, Money::ZERO);
    } else {
        assert!(spent > Money::ZERO, "an unsettled row is charged its hold");
    }
    assert_eq!(
        p.admission.budget().held(),
        Money::ZERO,
        "no hold outlives the call"
    );
}

/// An interpretation dropped mid-call releases its hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_interpretation_releases_its_hold() {
    let dir = tempfile::tempdir().expect("dir");
    corpus(dir.path());
    let p = parts(dir.path());
    *p.planner.hang_on.lock().expect("lock") = Some("# Corpus".into());
    let prices = prices();
    let clock = SystemClock;
    let asker = asker(&p);
    let run = RunId::new("r1");
    let pending = interpret_corpus(
        intake_parts(&p, &prices, &clock),
        &run,
        request(),
        &asker,
        &NoTestRuns,
    );
    let timed = tokio::time::timeout(Duration::from_millis(300), pending).await;
    assert!(timed.is_err(), "the call hangs and is cancelled");
    assert_eq!(p.planner.calls(), 1, "the call was in flight");
    assert_eq!(
        p.admission.budget().held(),
        Money::ZERO,
        "the hold was released"
    );
}

/// A call that
/// settles over its hold is charged, reported, and its answer is kept, same as
/// any other call. The overrun is an accounting fact about the estimate, not a
/// reason to discard a reading that already happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_overrun_interpretation_is_charged_and_its_answer_is_kept() {
    let dir = tempfile::tempdir().expect("dir");
    corpus(dir.path());
    let p = parts(dir.path());
    // Output priced so the fake's 50 output tokens cost far more than the hold,
    // which reserves one output token.
    let mut prices = prices();
    prices.set(
        PLAN_MODEL,
        ModelPrice {
            uncached_input: 1,
            cache_read: 1,
            cache_write: 1,
            output: 1_000_000_000,
            reasoning: 1_000_000_000,
        },
    );
    let clock = SystemClock;
    let asker = asker_with(p.planner.clone(), &p, 1);
    let intake = interpret_corpus(
        intake_parts(&p, &prices, &clock),
        &RunId::new("r1"),
        request(),
        &asker,
        &NoTestRuns,
    )
    .await;
    intake.result.expect("no cap refuses an overrun");
    assert_eq!(intake.charged.len(), 1);
    assert!(p.admission.budget().spent_run("r1") > Money::ZERO);
    assert_eq!(p.admission.budget().held(), Money::ZERO);
    assert_eq!(p.admission.budget().stats().overruns, 1);
    // Kept: the same corpus reads from the graph, no second call.
    let again = interpret_corpus(
        intake_parts(&p, &prices, &clock),
        &RunId::new("r1"),
        request(),
        &asker_with(p.planner.clone(), &p, 2_000),
        &NoTestRuns,
    )
    .await;
    let again = again.result.expect("stored");
    assert!(again.is_cached(), "the overrun answer was kept");
    assert_eq!(p.planner.calls(), 1, "the overrun answer was reused");
}
