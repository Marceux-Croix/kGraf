//! Templates (decision 0056) through the real loop: a completed run is mined into a
//! workflow template, the next run with a similar instruction is offered it in the
//! decomposition prompt, a blocked reuse marks it failed, and every run writes an
//! induced-demand record, a run that never got a plan included.

mod support;

use kgraf_core::{RunId, TokenUsage, estimate_tokens};
use kgraf_orchestrator::demand::{DemandRecord, summarize};
use kgraf_orchestrator::templates::{
    self, STANDING, STANDING_FAILED, STANDING_VERIFIED, TEMPLATE_SUBJECT, TemplateStep,
};
use kgraf_orchestrator::{
    ModelCaller, ModelDecomposer, Orchestrator, RunStatus, TraceEvent, TraceLog,
};
use kgraf_provider::{CallToken, Content, Provider, ProviderError, Request, Response, StopReason};
use std::sync::Arc;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

async fn trace_of(p: &Parts, run: &str) -> Vec<TraceEvent> {
    let log = TraceLog::open(&p.dir.join("runs").join(run).join("trace.jsonl"))
        .await
        .expect("trace");
    log.load().await.expect("load")
}

fn offered(trace: &[TraceEvent]) -> Vec<Vec<String>> {
    template_events(trace, "offered")
}

fn template_events(trace: &[TraceEvent], wanted: &str) -> Vec<Vec<String>> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Templates {
                action, templates, ..
            } if action == wanted => Some(templates.clone()),
            _ => None,
        })
        .collect()
}

fn records(p: &Parts) -> Vec<DemandRecord> {
    let g = p.graph.lock().expect("graph");
    DemandRecord::load_all(&g).expect("records")
}

fn standing(p: &Parts, id: &str) -> String {
    let g = p.graph.lock().expect("graph");
    let open = g
        .open_facts(STANDING, &format!("{TEMPLATE_SUBJECT}{id}"))
        .expect("standing");
    assert_eq!(open.len(), 1, "one open standing");
    open[0].object.clone()
}

fn parser_plan() -> String {
    plan_json(&[
        c("p", "write the config parser", "src/parser.rs", &[]),
        c("t", "test the config parser", "tests/parser.rs", &["p"]),
    ])
}

/// Services whose planner attributes its calls to run `id` (the shared harness
/// attributes every planner call to `r1`), as the CLI does: the demand record
/// counts a run's decomposition calls by that attribution.
fn services_for(p: &Parts, id: &str) -> kgraf_orchestrator::Services {
    let mut services = p.services(WINDOW);
    services.decomposer = Arc::new(ModelDecomposer(ModelCaller {
        provider: p.planner.clone(),
        ledger: p.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 2_000,
        agent: format!("orchestrator:{id}"),
        task: id.to_owned(),
    }));
    services
}

async fn run(p: &Parts, id: &str, instruction: &str) -> kgraf_orchestrator::RunReport {
    Orchestrator::new(config(), services_for(p, id))
        .expect("orchestrator")
        .start(RunId::new(id), instruction.into())
        .await
        .expect("run")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verified_decomposition_is_offered_to_the_next_similar_run_and_measured() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = parser_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );

    // Run 1: nothing to offer yet, so it is in the without-templates arm.
    let first = run(&p, "r1", "add a config parser module with tests").await;
    assert_eq!(first.status, RunStatus::Completed, "{first:?}");
    assert_eq!(first.analytics_error, None);
    let t1 = trace_of(&p, "r1").await;
    assert_eq!(
        offered(&t1),
        vec![Vec::<String>::new()],
        "an empty offer is recorded"
    );
    let mined = template_events(&t1, "mined");
    assert_eq!(mined.len(), 1, "the completed run was mined");
    let id = mined[0][0].clone();
    assert!(!p.planner.prompts()[0].contains("<workflow_template"));

    // Run 2: a similar instruction is offered run 1's template, as data, and its
    // plan has the template's structure, so it is a reuse.
    let second = run(&p, "r2", "add a yaml config parser with tests").await;
    assert_eq!(second.status, RunStatus::Completed, "{second:?}");
    let prompt = p.planner.prompts().last().cloned().expect("prompt");
    assert!(
        prompt.contains(&format!("<workflow_template id=\"{id}\"")),
        "the template reached the decomposer: {prompt}"
    );
    let t2 = trace_of(&p, "r2").await;
    assert_eq!(offered(&t2), vec![vec![id.clone()]]);
    assert_eq!(template_events(&t2, "reused"), vec![vec![id.clone()]]);
    assert!(
        template_events(&t2, "mined").is_empty(),
        "credited, not re-mined"
    );

    // Both runs are measured, one in each arm.
    let records = records(&p);
    assert_eq!(records.len(), 2);
    let r1 = records.iter().find(|r| r.run == "r1").expect("r1");
    let r2 = records.iter().find(|r| r.run == "r2").expect("r2");
    assert!(!r1.with_templates());
    assert_eq!(r2.templates_offered, vec![id.clone()]);
    assert_eq!(r2.template_matched.as_deref(), Some(id.as_str()));
    for r in [r1, r2] {
        assert_eq!(r.status, "completed");
        assert_eq!(r.contracts, 2);
        assert_eq!(r.units_started, 2, "one worker per contract");
        assert_eq!(r.decomposition_calls, 1);
        assert!(r.decomposition_spend_micros > 0, "priced");
        assert!(r.spend_micros >= r.decomposition_spend_micros);
    }
    let summary = summarize(&records);
    assert_eq!((summary.with.runs, summary.without.runs), (1, 1));

    // The template has run 2 as a second success.
    let g = p.graph.lock().expect("graph");
    let shape = g
        .open_facts(templates::SHAPE, &format!("{TEMPLATE_SUBJECT}{id}"))
        .expect("shape");
    assert_eq!(shape[0].episode_count, 2);
    assert_eq!(shape[0].episodes, vec!["r1", "r2"]);
    // Folded into the demand records: no per-run scratch keys remain.
    for key in ["frugality/templates/r2", "frugality/plan/r2"] {
        assert_eq!(g.meta(key).expect("meta"), None, "{key}");
    }
}

/// The blocked path through the loop. A run that reuses a template and ends
/// blocked closes the template's `verified` standing, and the template is no
/// longer offered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_reuse_marks_the_template_failed_and_it_is_no_longer_offered() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = parser_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let first = run(&p, "r1", "add a config parser module with tests").await;
    assert_eq!(first.status, RunStatus::Completed, "{first:?}");
    let id = template_events(&trace_of(&p, "r1").await, "mined")[0][0].clone();
    assert_eq!(standing(&p, &id), STANDING_VERIFIED);

    // Every verification now fails, so run 2 ends blocked.
    *p.verifier.require.lock().expect("lock") = Some("never written".into());
    let second = run(&p, "r2", "add a yaml config parser with tests").await;
    assert!(
        matches!(second.status, RunStatus::Blocked { .. }),
        "{second:?}"
    );
    let t2 = trace_of(&p, "r2").await;
    assert_eq!(
        template_events(&t2, "failed_on_reuse"),
        vec![vec![id.clone()]]
    );
    assert_eq!(standing(&p, &id), STANDING_FAILED);

    // Run 3 is offered nothing, and the blocked run kept its resumable state.
    *p.verifier.require.lock().expect("lock") = None;
    let _ = run(&p, "r3", "add a yaml config parser with tests").await;
    assert_eq!(
        offered(&trace_of(&p, "r3").await),
        vec![Vec::<String>::new()]
    );
    let g = p.graph.lock().expect("graph");
    assert!(g.meta("frugality/plan/r2").expect("meta").is_some());
}

/// A run whose decomposition fails is measured too, with the failed call
/// counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_never_gets_a_plan_still_writes_a_demand_record() {
    let dir = tempfile::tempdir().expect("dir");
    let p = Parts::open(
        dir.path(),
        |_| "this is not a plan".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let err = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "add a parser".into())
        .await
        .expect_err("no plan");
    assert!(err.to_string().contains("decomposition"), "{err}");
    let records = records(&p);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, "plan_failed");
    assert_eq!(
        records[0].decomposition_calls, 3,
        "the failed calls count: the first ask and two more"
    );
    assert_eq!(records[0].units_started, 0);
}

/// A planner that bills the prompt it was sent: its input tokens are the
/// prompt's estimated size, so a hold that ignored part of the prompt overruns.
struct BillsPrompt(String);

#[async_trait::async_trait]
impl Provider for BillsPrompt {
    fn adapter(&self) -> &'static str {
        "bills-prompt"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let prompt: String = request
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(Content::as_text)
            .collect::<Vec<_>>()
            .join("\n");
        let usage = TokenUsage {
            uncached_input: estimate_tokens(&prompt),
            text_output: 50,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            model: request.model.clone(),
            content: vec![Content::text(self.0.clone())],
            stop_reason: StopReason::EndTurn,
            usage,
            request_id: None,
        })
    }
}

/// The decomposition's hold covers the offered templates, so a large
/// template does not make the call overrun its hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_decomposition_hold_covers_a_large_offered_template() {
    let dir = tempfile::tempdir().expect("dir");
    let p = Parts::open(
        dir.path(),
        |_| String::new(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let instruction = "add a config parser module with tests";
    // About 100,000 tokens of template text.
    let big = TemplateStep {
        instruction: "x".repeat(300_000),
        acceptance: "tests pass".into(),
        writes: vec!["src/file.rs".into()],
        depends_on: vec![],
        tools: vec![],
    };
    let t = templates::from_steps(instruction, vec![big]).expect("template");
    {
        let g = p.graph.lock().expect("graph");
        templates::record_success(&g, &t, "seed", 1).expect("seed");
    }
    let mut services = p.services(WINDOW);
    services.decomposer = Arc::new(ModelDecomposer(ModelCaller {
        provider: Arc::new(BillsPrompt(parser_plan())),
        ledger: p.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 2_000,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    }));
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), instruction.into())
        .await
        .expect("run");
    assert_eq!(offered(&trace_of(&p, "r1").await), vec![vec![t.id.clone()]]);
    let r = &records(&p)[0];
    assert!(
        r.decomposition_spend_micros > 0 && r.decomposition_calls == 1,
        "{r:?}"
    );
    assert_eq!(report.counters.overruns, 0, "{report:?}");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
}

/// Live occupancy is in the status reply text, not only the structured
/// field (a channel shows only the text).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_reply_text_carries_live_context_occupancy() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().expect("dir");
    // COSTLY reports 50,000 input tokens in round 0; HANG then holds round 1.
    let plan = plan_json(&[c("a", "make a COSTLY HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), services_for(&p, "r1")).expect("orchestrator");
    let handle = o.handle();
    let running = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    while !(p.alpha.hang_reached.load(Ordering::SeqCst)
        || p.beta.hang_reached.load(Ordering::SeqCst))
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The round-0 usage event reaches the board asynchronously, so a status
    // taken the instant round 1 starts may still show 0: wait for it (bounded).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        let status = handle.status().await.expect("status");
        if status.occupancy.iter().any(|o| o.context_tokens > 0) {
            break status;
        }
        assert!(std::time::Instant::now() < deadline, "{status:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(status.occupancy.len(), 1, "{status:?}");
    let o = &status.occupancy[0];
    assert_eq!(o.context_tokens, 50_000);
    let percent = o.percent_of_stop().expect("a stop point");
    assert!(
        status
            .text
            .contains(&format!("{} {percent}% of stop point", o.worker)),
        "{}",
        status.text
    );
    handle.stop().await;
    let _ = running.await;
}

/// Resuming a completed run is refused,
/// and its demand record is left exactly as the run wrote it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_resume_of_a_completed_run_keeps_its_demand_record() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = parser_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let first = run(&p, "r1", "add a config parser module with tests").await;
    assert_eq!(first.status, RunStatus::Completed);
    let second = run(&p, "r2", "add a yaml config parser with tests").await;
    assert_eq!(second.status, RunStatus::Completed);
    let before = records(&p).into_iter().find(|r| r.run == "r2").expect("r2");
    let calls_before = p.ledger.calls().expect("calls").len();
    let resumed = Orchestrator::new(config(), services_for(&p, "r2"))
        .expect("orchestrator")
        .resume(RunId::new("r2"))
        .await;
    let err = resumed.expect_err("a completed run is not resumed");
    assert!(err.to_string().contains("already completed"), "{err}");
    let after = records(&p).into_iter().find(|r| r.run == "r2").expect("r2");
    assert_eq!(before, after, "the record is untouched");
    assert_eq!(before.templates_offered.len(), 1, "arm kept");
    assert_eq!(before.contracts, 2, "contracts kept");
    assert_eq!(p.ledger.calls().expect("calls").len(), calls_before);
}

/// Pre-existing on main: resuming in the process that ran the run charged the
/// run's ledger spend to the budget a second time. Seeding is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_process_resume_does_not_charge_the_runs_spend_again() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = parser_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    // Every verification fails, so the run ends blocked, and is resumable.
    *p.verifier.require.lock().expect("lock") = Some("never written".into());
    let first = run(&p, "r1", "add a config parser module with tests").await;
    assert!(
        matches!(first.status, RunStatus::Blocked { .. }),
        "{first:?}"
    );
    let spent = p.admission.budget().spent_run("r1");
    assert!(spent.micros() > 0);
    let calls = p.ledger.calls().expect("calls").len();

    let again = Orchestrator::new(config(), services_for(&p, "r1"))
        .expect("orchestrator")
        .resume(RunId::new("r1"))
        .await
        .expect("resume");
    assert!(
        matches!(again.status, RunStatus::Blocked { .. }),
        "{again:?}"
    );
    assert_eq!(
        p.ledger.calls().expect("calls").len(),
        calls,
        "the resume made no new call"
    );
    assert_eq!(
        p.admission.budget().spent_run("r1"),
        spent,
        "no new call, no new spend"
    );
}

/// Through the loop: a run offered a template whose plan has the same
/// structure but unrelated step text, and which ends blocked, does not demote
/// the template (nor would its success credit it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unrelated_plan_of_the_same_structure_does_not_demote_the_template() {
    let dir = tempfile::tempdir().expect("dir");
    let parser = parser_plan();
    let unrelated = plan_json(&[
        c("p", "render galaxy nodes", "src/parser.rs", &[]),
        c("t", "benchmark layout speed", "tests/parser.rs", &["p"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.contains("yaml") {
                unrelated.clone()
            } else {
                parser.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let first = run(&p, "r1", "add a config parser module with tests").await;
    assert_eq!(first.status, RunStatus::Completed, "{first:?}");
    let id = template_events(&trace_of(&p, "r1").await, "mined")[0][0].clone();

    *p.verifier.require.lock().expect("lock") = Some("never written".into());
    let second = run(&p, "r2", "add a yaml config parser with tests").await;
    assert!(
        matches!(second.status, RunStatus::Blocked { .. }),
        "{second:?}"
    );
    let t2 = trace_of(&p, "r2").await;
    assert_eq!(offered(&t2), vec![vec![id.clone()]], "it was offered");
    assert!(template_events(&t2, "failed_on_reuse").is_empty());
    assert_eq!(standing(&p, &id), STANDING_VERIFIED, "not demoted");
    let r2 = records(&p).into_iter().find(|r| r.run == "r2").expect("r2");
    assert_eq!(r2.template_matched, None);
}

/// An analytics failure at the end of a run is reported with the run and
/// recorded in its trace; the run still completes and is not left active.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_analytics_failure_is_traced_and_reported_not_raised() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = parser_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    // A corrupt earlier demand record for r1 makes writing r1's record fail.
    {
        let g = p.graph.lock().expect("graph");
        let old = DemandRecord {
            run: "r1".into(),
            status: "stopped".into(),
            templates_offered: vec![],
            template_matched: None,
            contracts: 0,
            dispatches: 0,
            units_started: 0,
            verified: 0,
            decomposition_calls: 0,
            decomposition_spend_micros: 0,
            spend_micros: 0,
            unknown_spend_calls: 0,
        };
        old.store(&g, 1).expect("store");
    }
    let raw = rusqlite::Connection::open(dir.path().join("graph.db")).expect("raw");
    raw.execute(
        "UPDATE facts SET episodes = 'corrupt' WHERE subject = 'run:r1'",
        [],
    )
    .expect("corrupt");
    drop(raw);

    let report = run(&p, "r1", "add a config parser module with tests").await;
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let error = report.analytics_error.clone().expect("reported");
    assert!(error.contains("graph"), "{error}");
    let trace = trace_of(&p, "r1").await;
    assert!(
        trace.iter().any(|e| matches!(
            e,
            TraceEvent::Templates { action, detail, .. }
                if action == "analytics_failed" && *detail == error
        )),
        "traced"
    );
    let g = p.graph.lock().expect("graph");
    assert_eq!(
        g.meta("orchestrator/active/r1").expect("meta").as_deref(),
        Some(""),
        "the run is not left active"
    );
}

/// The ledger's settled spend of every call attributed to `run`.
fn ledger_spend(p: &Parts, run: &str) -> kgraf_core::Money {
    let ids: Vec<kgraf_core::CallId> = p
        .ledger
        .calls_under_task(run, None)
        .expect("calls")
        .into_iter()
        .map(|c| c.id)
        .collect();
    let spend = p.ledger.spend_of(&ids, &prices()).expect("spend");
    assert!(spend.unknown.is_empty(), "{:?}", spend.unknown);
    spend.total
}

/// The verifier's partial-seeding probe, ported: spend the book lacks, for a
/// task it knows and for one it never saw, is added on a same-process resume,
/// exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skeptic_partial_seed() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = parser_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    *p.verifier.require.lock().expect("lock") = Some("never written".into());
    let first = run(&p, "r1", "add a config parser module with tests").await;
    assert!(
        matches!(first.status, RunStatus::Blocked { .. }),
        "{first:?}"
    );
    let run_before = p.admission.budget().spent_run("r1");
    let p_before = p.admission.budget().spent_task("r1/p");
    assert!(p_before.micros() > 0);
    // Calls the book never saw: one on a task it knows, one on a new task.
    let ledger = p.ledger.clone();
    tokio::task::spawn_blocking(move || {
        for (task, input) in [("r1/p", 1_000u64), ("r1/x", 500u64)] {
            let t = ledger
                .begin(
                    "probe",
                    kgraf_core::Protocol::Additive,
                    PLAN_MODEL,
                    "probe-agent",
                    task,
                )
                .expect("begin");
            t.settle(
                &TokenUsage {
                    uncached_input: input,
                    ..TokenUsage::default()
                },
                None,
            )
            .expect("settle");
        }
    })
    .await
    .expect("join");
    let price = |input: u64| {
        prices()
            .cost(
                PLAN_MODEL,
                &TokenUsage {
                    uncached_input: input,
                    ..TokenUsage::default()
                },
            )
            .expect("priced")
    };
    for i in 0..2 {
        let again = tokio::time::timeout(
            Duration::from_secs(20),
            Orchestrator::new(config(), services_for(&p, "r1"))
                .expect("orchestrator")
                .resume(RunId::new("r1")),
        )
        .await
        .expect("resume timed out")
        .expect("resume");
        assert!(
            matches!(again.status, RunStatus::Blocked { .. }),
            "{again:?}"
        );
        let book = p.admission.budget();
        assert_eq!(
            book.spent_task("r1/p").micros(),
            p_before.micros() + price(1_000).micros(),
            "resume {i}: the known task gains only the missing call"
        );
        assert_eq!(book.spent_task("r1/x"), price(500), "resume {i}: new task");
        assert_eq!(
            book.spent_run("r1").micros(),
            run_before.micros() + price(1_000).micros() + price(500).micros(),
            "resume {i}: added once"
        );
    }
    assert_eq!(p.admission.budget().spent_run("r1"), ledger_spend(&p, "r1"));
}

/// Review item 4: a failed orchestrator call is charged at its ledger cost, not
/// its whole hold, so the book matches the ledger and a same-process resume
/// still seeds what is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_decomposition_is_charged_its_ledger_cost_not_its_hold() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().expect("dir");
    let plan = parser_plan();
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = asked.clone();
    let p = Parts::open(
        dir.path(),
        move |_| {
            // Bad on the first ask and both re-asks, so the run fails.
            if counter.fetch_add(1, Ordering::SeqCst) < 3 {
                "this is not a plan".into()
            } else {
                plan.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let failed = Orchestrator::new(config(), services_for(&p, "r1"))
        .expect("orchestrator")
        .start(
            RunId::new("r1"),
            "add a config parser module with tests".into(),
        )
        .await;
    assert!(failed.is_err(), "no answer of the first three is a plan");
    let charged = p.admission.budget().spent_run("r1");
    let billed = ledger_spend(&p, "r1");
    assert!(billed.micros() > 0, "the bad answer was billed");
    assert_eq!(charged, billed, "charged the ledger cost, not the hold");

    let resumed = Orchestrator::new(config(), services_for(&p, "r1"))
        .expect("orchestrator")
        .resume(RunId::new("r1"))
        .await
        .expect("resume");
    assert_eq!(resumed.status, RunStatus::Completed, "{resumed:?}");
    assert_eq!(
        p.admission.budget().spent_run("r1"),
        ledger_spend(&p, "r1"),
        "book and ledger agree after the resume"
    );
}

/// N1b: a run that completes without a plan snapshot writes no demand record
/// (it would be false) and says so in the trace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_run_without_a_plan_snapshot_records_nothing() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), services_for(&p, "r1")).expect("orchestrator");
    let handle = o.handle();
    let running = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    while !(p.alpha.hang_reached.load(Ordering::SeqCst)
        || p.beta.hang_reached.load(Ordering::SeqCst))
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    handle.stop().await;
    let stopped = running.await.expect("join").expect("run");
    assert_eq!(stopped.status, RunStatus::Stopped);
    let before = records(&p);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].status, "stopped");
    // The snapshot is gone (a run from before snapshots existed).
    p.graph
        .lock()
        .expect("graph")
        .clear_meta("frugality/plan/r1")
        .expect("clear");

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    let done = Orchestrator::new(config(), services_for(&p, "r1"))
        .expect("orchestrator")
        .resume(RunId::new("r1"))
        .await
        .expect("resume");
    assert_eq!(done.status, RunStatus::Completed, "{done:?}");
    assert_eq!(done.analytics_error, None);
    assert_eq!(records(&p), before, "no record written from missing state");
    assert!(
        !template_events(&trace_of(&p, "r1").await, "not_recorded").is_empty(),
        "the trace says why"
    );
}

/// Review item 3 through the loop: an unreadable template is quarantined and
/// reported; the run still plans and completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_template_is_quarantined_and_the_run_still_plans() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = parser_plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let instruction = "add a config parser module with tests";
    let step = TemplateStep {
        instruction: "write the config parser".into(),
        acceptance: "tests pass".into(),
        writes: vec!["src/file.rs".into()],
        depends_on: vec![],
        tools: vec![],
    };
    let t = templates::from_steps(instruction, vec![step]).expect("template");
    {
        let g = p.graph.lock().expect("graph");
        templates::record_success(&g, &t, "seed", 1).expect("seed");
        g.assert_fact(
            &kgraf_graph::NewFact {
                subject: &format!("{TEMPLATE_SUBJECT}{}", t.id),
                predicate: templates::SHAPE,
                object: "{not json",
                valid_at: 2,
                invalid_at: None,
                episode: "corrupt",
            },
            2,
        )
        .expect("corrupt");
    }
    let report = run(&p, "r1", instruction).await;
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains(&t.id) && w.contains("quarantined")),
        "{:?}",
        report.warnings
    );
    let trace = trace_of(&p, "r1").await;
    assert_eq!(
        template_events(&trace, "quarantined"),
        vec![vec![t.id.clone()]]
    );
    assert_eq!(standing(&p, &t.id), templates::STANDING_QUARANTINED);
}

/// The status text and the occupancy figures came from separate reads of the
/// board, so a usage event landing between them made the text disagree with
/// the struct (the flaky failure of the test above under load). Both now come
/// from one snapshot. Stress: 20 runs, and in each 10 status replies taken
/// across the moment the round-0 usage lands, 200 replies in all; every reply's
/// text must carry exactly each worker's figure from the same reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_text_and_occupancy_always_agree_under_stress() {
    use std::sync::atomic::Ordering;
    for _ in 0..20 {
        let dir = tempfile::tempdir().expect("dir");
        let plan = plan_json(&[c("a", "make a COSTLY HANG", "src/a.rs", &[])]);
        let p = Parts::open(
            dir.path(),
            move |_| plan.clone(),
            |_| SERVES.into(),
            Duration::from_millis(5),
        );
        p.alpha.hang.store(true, Ordering::SeqCst);
        p.beta.hang.store(true, Ordering::SeqCst);
        let o = Orchestrator::new(config(), services_for(&p, "r1")).expect("orchestrator");
        let handle = o.handle();
        let running = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
        let mut replies = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while replies < 10 {
            assert!(std::time::Instant::now() < deadline, "no status replies");
            let Some(status) = handle.status().await else {
                continue;
            };
            for o in &status.occupancy {
                let expected = match o.percent_of_stop() {
                    Some(p) => format!("{} {p}% of stop point", o.worker),
                    None => format!("{} unknown", o.worker),
                };
                assert!(
                    status.text.contains(&expected),
                    "text {:?} disagrees with {o:?}",
                    status.text
                );
            }
            if !status.occupancy.is_empty() {
                replies += 1;
            }
        }
        handle.stop().await;
        let _ = running.await;
    }
}
