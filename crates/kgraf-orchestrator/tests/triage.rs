//! Small requests skip planning (decision 0100, section 6), through the real
//! loop: the rule's three answers, the one triage call and its fallbacks, early
//! start with its merge hold and its cancel-then-plan order, resume in the
//! middle of a triage, plan review, templates, and the background root gate.
//!
//! Every test runs with `--plan auto`; a control run with the default (`full`)
//! shows the same instruction planned by the planner, so each assertion on
//! the small path is one the old path fails.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::triage::{PlanMode, SmallRequestConfig};
use kgraf_orchestrator::{
    GateRequest, GateRunner, GateVerdict, Orchestrator, OrchestratorConfig, RunStatus, RunStore,
    TraceEvent, TraceLog, UntrustedText,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;
const TRIAGE: &str = "Triage request:";

fn auto(early_start: bool) -> OrchestratorConfig {
    let mut c = config();
    c.small = SmallRequestConfig {
        plan: PlanMode::Auto,
        early_start,
    };
    c
}

fn with_src(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("src")).expect("src");
    std::fs::create_dir_all(dir.join("docs")).expect("docs");
}

/// Orchestrator ledger calls by kind.
fn planner_kinds(p: &Parts) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for call in p.ledger.calls().expect("calls") {
        if call.agent_id.starts_with("orchestrator:") {
            *out.entry(call.trace.kind.unwrap_or_default()).or_default() += 1;
        }
    }
    out
}

fn kind(p: &Parts, k: &str) -> usize {
    planner_kinds(p).get(k).copied().unwrap_or(0)
}

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

/// `f`, which must finish within 60 s: a run that never ends fails the test
/// instead of hanging it.
async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .expect("finished within 60 s")
}

fn record_of(p: &Parts) -> kgraf_orchestrator::RunStateRecord {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
}

fn triage_steps(trace: &[TraceEvent]) -> Vec<String> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Triage { step, .. } => Some(step.clone()),
            _ => None,
        })
        .collect()
}

fn index_of(trace: &[TraceEvent], f: impl Fn(&TraceEvent) -> bool) -> Option<usize> {
    trace.iter().position(f)
}

fn triage_step(step: &'static str) -> impl Fn(&TraceEvent) -> bool {
    move |e| matches!(e, TraceEvent::Triage { step: s, .. } if s == step)
}

fn latency_kinds(trace: &[TraceEvent]) -> Vec<String> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Latency(l) => Some(
                serde_json::to_value(l.kind)
                    .expect("json")
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ),
            _ => None,
        })
        .collect()
}

fn plan_contracts(trace: &[TraceEvent]) -> Vec<Vec<String>> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::PlanAccepted { contracts, .. } => {
                Some(contracts.iter().map(|c| c.id.clone()).collect())
            }
            _ => None,
        })
        .collect()
}

/// A request the rule sizes small runs as one contract with no planner call:
/// zero `decompose`, `interfaces` and `triage` rows, one contract on the
/// graph (one `plan_accepted`), and B2 measured from the request. The same
/// request planned in full makes a `decompose` call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_small_request_runs_one_contract_with_no_planner_call() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let plan = plan_json(&[c("p", "make it", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(auto(false), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "Fix the greeting WRITE src/a.rs".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["c1".to_owned()]);
    assert_eq!(p.planner.calls(), 0, "{:?}", p.planner.prompts());
    for k in ["decompose", "interfaces", "triage"] {
        assert_eq!(kind(&p, k), 0, "{k}: {:?}", planner_kinds(&p));
    }
    let trace = trace_of(&p).await;
    assert_eq!(plan_contracts(&trace), vec![vec!["c1".to_owned()]]);
    assert_eq!(triage_steps(&trace)[0], "decision");
    assert!(
        latency_kinds(&trace).contains(&"small_request_dispatch".to_owned()),
        "{:?}",
        latency_kinds(&trace)
    );
    let record = record_of(&p);
    let t = record.triage.expect("triage record");
    assert!(t.small_path);
    assert!(record.unverified.is_empty());

    // Control: the same request planned in full asks the planner.
    let dir2 = tempfile::tempdir().expect("dir");
    with_src(dir2.path());
    let plan = plan_json(&[c("p", "make it", "src/a.rs", &[])]);
    let full = Parts::open(
        dir2.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(config(), full.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "Fix the greeting WRITE src/a.rs".into())
        .await
        .expect("run");
    assert_eq!(report.verified, vec!["p".to_owned()]);
    assert_eq!(kind(&full, "decompose"), 1);
    assert!(record_of(&full).triage.is_none());
}

/// The rule cannot size it; the one triage call says "one": one `triage`
/// row, no `decompose`, the contract writes the answer's file, and B3 is
/// measured.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_answer_runs_one_contract_after_one_triage_call() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        |prompt| {
            if prompt.starts_with(TRIAGE) {
                r#"{"size":"one","writes":["src/a.rs"]}"#.into()
            } else {
                "not a plan".into()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(auto(false), p.services(WINDOW))
        .expect("orchestrator")
        .start(
            RunId::new("r1"),
            "WRITE src/a.rs, then tidy src/b.rs and docs/x.md".into(),
        )
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["c1".to_owned()]);
    assert_eq!(kind(&p, "triage"), 1);
    assert_eq!(kind(&p, "decompose"), 0);
    let trace = trace_of(&p).await;
    let steps = triage_steps(&trace);
    for s in ["decision", "asked", "early_unmet", "answer", "one_contract"] {
        assert!(steps.contains(&s.to_owned()), "{s}: {steps:?}");
    }
    assert!(
        latency_kinds(&trace).contains(&"triage_dispatch".to_owned()),
        "{:?}",
        latency_kinds(&trace)
    );
    let writes: Vec<String> = trace
        .iter()
        .find_map(|e| match e {
            TraceEvent::PlanAccepted { contracts, .. } => Some(contracts[0].writes.clone()),
            _ => None,
        })
        .expect("plan");
    assert_eq!(writes, vec!["src/a.rs".to_owned()]);
}

/// "split" plans the request in full, with a notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_split_answer_plans_in_full_with_a_notice() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let plan = plan_json(&[c("p1", "make p", "src/p.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.starts_with(TRIAGE) {
                r#"{"size":"split"}"#.into()
            } else {
                plan.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(auto(false), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "Fix the parser".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["p1".to_owned()]);
    assert_eq!(kind(&p, "triage"), 1);
    assert_eq!(kind(&p, "decompose"), 1);
    assert!(
        report.notices.iter().any(|n| n.contains("planned in full")),
        "{:?}",
        report.notices
    );
}

/// A malformed answer is not asked again: exactly one `triage` row, then the
/// full plan, with the failure as a notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_answer_is_one_row_and_a_full_plan() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let plan = plan_json(&[c("p1", "make p", "src/p.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.starts_with(TRIAGE) {
                "I think one worker can do it".into()
            } else {
                plan.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = Orchestrator::new(auto(false), p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "Fix the parser".into())
        .await
        .expect("run");
    assert_eq!(report.verified, vec!["p1".to_owned()]);
    assert_eq!(kind(&p, "triage"), 1, "never asked again");
    assert_eq!(kind(&p, "decompose"), 1);
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("the triage call failed")),
        "{:?}",
        report.notices
    );
    let record = record_of(&p);
    assert!(matches!(
        record.triage.and_then(|t| t.settled),
        Some(kgraf_orchestrator::triage::Settled::Failed { .. })
    ));
}

/// Blocks the planner's triage answer until `release` is set (bounded).
fn wait_for(release: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Early start: the worker's first request leaves before the triage answer;
/// it verifies, but its merge waits for the answer ("merge_held"), and merges
/// after "one".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_early_worker_verifies_but_merges_only_after_the_answer() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let release = Arc::new(AtomicBool::new(false));
    let r = release.clone();
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.starts_with(TRIAGE) {
                wait_for(&r);
                r#"{"size":"one","writes":["src/a.rs"]}"#.into()
            } else {
                "not a plan".into()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let o = Orchestrator::new(auto(true), p.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(o.start(
        RunId::new("r1"),
        "WRITE src/a.rs, then tidy src/b.rs and src/c.rs".into(),
    ));
    until("the early worker was verified", || {
        p.verifier.started.load(Ordering::SeqCst) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    release.store(true, Ordering::SeqCst);
    let report = bounded(run).await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["c1".to_owned()]);
    assert_eq!(kind(&p, "triage"), 1);
    assert_eq!(kind(&p, "decompose"), 0);
    let trace = trace_of(&p).await;
    let held = index_of(&trace, triage_step("merge_held")).expect("merge held");
    let answer = index_of(&trace, triage_step("answer")).expect("answer");
    let released = index_of(&trace, triage_step("merge_released")).expect("released");
    let merged = index_of(&trace, |e| {
        matches!(e, TraceEvent::Integration { contract, outcome, .. }
            if contract == "c1" && outcome != "check_base")
    })
    .expect("merged");
    assert!(
        held < answer && answer < released && released < merged,
        "{trace:#?}"
    );
    // The worker's first request left before the triage call ended.
    let calls = p.ledger.calls().expect("calls");
    let triage_end = calls
        .iter()
        .find(|c| c.trace.kind.as_deref() == Some("triage"))
        .and_then(|c| c.finished_ms)
        .expect("triage row");
    let worker_start = calls
        .iter()
        .filter(|c| c.task_id == "r1/c1")
        .map(|c| c.started_ms)
        .min()
        .expect("worker row");
    assert!(worker_start <= triage_end, "{worker_start} > {triage_end}");
    assert!(latency_kinds(&trace).contains(&"triage_dispatch".to_owned()));
}

/// "split" stops the early worker, releases its worktree and leases and
/// records its usage, then plans: the cancel is traced before the full plan
/// is accepted, the early worker's calls ended before the plan call began,
/// and nothing of the early contract merges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_split_cancels_the_early_worker_before_the_plan_is_made() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let release = Arc::new(AtomicBool::new(false));
    let r = release.clone();
    let plan = plan_json(&[c("p1", "make p", "src/p.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.starts_with(TRIAGE) {
                wait_for(&r);
                r#"{"size":"split"}"#.into()
            } else {
                plan.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    // The early worker is held in its first call, so the split cuts it.
    let _held = p.hold_workers("src/a.rs");
    let o = Orchestrator::new(auto(true), p.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(o.start(
        RunId::new("r1"),
        "WRITE src/a.rs, then tidy src/b.rs and src/c.rs".into(),
    ));
    until("the early worker's first request left", || {
        !p.stats.first_requests.lock().expect("lock").is_empty()
    })
    .await;
    release.store(true, Ordering::SeqCst);
    let report = bounded(run).await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["p1".to_owned()]);
    assert_eq!(kind(&p, "triage"), 1);
    assert_eq!(kind(&p, "decompose"), 1);
    let trace = trace_of(&p).await;
    let cancelled = index_of(&trace, triage_step("early_cancelled")).expect("cancelled");
    let plans: Vec<usize> = trace
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, TraceEvent::PlanAccepted { .. }))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(plans.len(), 2, "the early contract, then the full plan");
    assert!(
        cancelled < plans[1],
        "cancelled at {cancelled}, planned at {}",
        plans[1]
    );
    assert!(
        !trace.iter().any(
            |e| matches!(e, TraceEvent::Integration { contract, outcome, .. }
            if contract == "c1" && outcome == "merged")
        ),
        "nothing of the early contract merges"
    );
    let calls = p.ledger.calls().expect("calls");
    let plan_start = calls
        .iter()
        .filter(|c| c.trace.kind.as_deref() == Some("decompose"))
        .map(|c| c.started_ms)
        .min()
        .expect("decompose row");
    for c in calls.iter().filter(|c| c.task_id == "r1/c1") {
        let end = c.finished_ms.expect("the early worker's call is closed");
        assert!(end <= plan_start, "{end} > {plan_start}");
    }
    let record = record_of(&p);
    let t = record.triage.expect("triage");
    let cancel = t.early_cancelled.expect("usage recorded");
    assert_eq!(cancel.contract, "c1");
    assert!(
        record.admitted_leases.is_empty(),
        "{:?}",
        record.admitted_leases
    );
    assert!(
        record
            .coordination
            .closed
            .iter()
            .any(|c| c.contract == "c1"),
        "the early contract is closed"
    );
}

/// Short leases, so a killed process's run lease lapses in the test's time.
fn restart_cfg(early: bool) -> OrchestratorConfig {
    let mut c = auto(early);
    c.lease_ttl_ms = 300;
    c
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn wait_blocking(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Killed while the triage call is out, with or without an early worker: the
/// resumed run makes no second triage call, starts no second early worker,
/// cancels the first, and plans in full.
fn resume_mid_triage(early: bool) {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let first = runtime();
    let p1 = Parts::open_with(
        dir.path(),
        |_| "never answered".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    *p1.planner.hang_on.lock().expect("lock") = Some(TRIAGE.into());
    let _held = p1.hold_workers("src/a.rs");
    let o = Orchestrator::new(restart_cfg(early), p1.services(WINDOW)).expect("orchestrator");
    first.spawn(o.start(
        RunId::new("r1"),
        "WRITE src/a.rs, then tidy src/b.rs and src/c.rs".into(),
    ));
    wait_blocking("the triage call was asked", || p1.planner.calls() >= 1);
    if early {
        wait_blocking("the early worker started", || {
            !p1.stats.first_requests.lock().expect("lock").is_empty()
        });
    }
    first.shutdown_background();
    drop(p1);
    std::thread::sleep(Duration::from_millis(450));

    let second = runtime();
    let plan = plan_json(&[c("p1", "make p", "src/p.rs", &[])]);
    let asked_triage = Arc::new(AtomicUsize::new(0));
    let asked = asked_triage.clone();
    let p2 = Parts::open_with(
        dir.path(),
        move |prompt| {
            if prompt.starts_with(TRIAGE) {
                asked.fetch_add(1, Ordering::SeqCst);
            }
            plan.clone()
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let report = second
        .block_on(bounded(async {
            Orchestrator::new(restart_cfg(early), p2.services(WINDOW))
                .expect("orchestrator")
                .resume(RunId::new("r1"))
                .await
        }))
        .expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["p1".to_owned()]);
    assert_eq!(
        asked_triage.load(Ordering::SeqCst),
        0,
        "no second triage call"
    );
    let early_again = p2
        .stats
        .openings
        .lock()
        .expect("lock")
        .iter()
        .filter(|o| o.contains("src/a.rs"))
        .count();
    assert_eq!(early_again, 0, "no second early worker");
    let trace = second.block_on(trace_of(&p2));
    let steps = triage_steps(&trace);
    assert!(steps.contains(&"lost".to_owned()), "{steps:?}");
    assert_eq!(
        steps.contains(&"early_cancelled".to_owned()),
        early,
        "{steps:?}"
    );
    let record = record_of(&p2);
    assert_eq!(
        record.triage.and_then(|t| t.settled),
        Some(kgraf_orchestrator::triage::Settled::Lost)
    );
}

#[test]
fn a_restart_during_triage_plans_in_full_with_no_second_call() {
    resume_mid_triage(false);
}

#[test]
fn a_restart_during_an_early_start_cancels_it_and_starts_no_second_worker() {
    resume_mid_triage(true);
}

/// Plan review in an asking mode shows the one contract as the card, before
/// any planner call; the user's yes runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_review_shows_the_one_contract_as_its_card() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        |_| "not a plan".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let control = Arc::new(kgraf_tools::PermissionControl::manual());
    control.set_plan_review(true, kgraf_tools::ModeSource::UserInterface);
    let o = Orchestrator::new(auto(false), p.services(WINDOW))
        .expect("orchestrator")
        .with_permissions(control)
        .with_question_attendance(kgraf_orchestrator::QuestionAttendance::Box);
    let desk = o.review_desk();
    let run = tokio::spawn(o.start(RunId::new("r1"), "Fix the greeting WRITE src/a.rs".into()));
    until("the plan card is shown", || desk.pending_plan().is_some()).await;
    let view = desk.pending_plan().expect("card");
    assert_eq!(view.items.len(), 1);
    assert_eq!(view.items[0].contract, "c1");
    assert_eq!(view.items[0].writes, vec!["src/a.rs".to_owned()]);
    assert_eq!(p.planner.calls(), 0, "no planner call behind the card");
    desk.answer_plan(kgraf_orchestrator::PlanAnswer::Approve)
        .expect("approve");
    let report = bounded(run).await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["c1".to_owned()]);
    assert_eq!(kind(&p, "decompose"), 0);
}

/// `OrchestratorDecides` and `Bypass` never ask: with plan review on, the
/// small request runs with no card, and the skip is traced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_modes_that_never_ask_skip_the_card() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        |_| "not a plan".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let control = Arc::new(kgraf_tools::PermissionControl::manual());
    control.set_plan_review(true, kgraf_tools::ModeSource::UserInterface);
    control
        .set_mode(
            kgraf_tools::PermissionMode::OrchestratorDecides,
            kgraf_tools::ModeSource::UserInterface,
        )
        .expect("mode");
    let o = Orchestrator::new(auto(false), p.services(WINDOW))
        .expect("orchestrator")
        .with_permissions(control)
        .with_question_attendance(kgraf_orchestrator::QuestionAttendance::Box);
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        o.start(RunId::new("r1"), "Fix the greeting WRITE src/a.rs".into()),
    )
    .await
    .expect("no card waited")
    .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let trace = trace_of(&p).await;
    assert!(trace.iter().any(|e| matches!(e,
        TraceEvent::PlanReview { outcome, .. } if outcome == "skipped")));
}

/// A verified one-step template whose write patterns are the named files
/// decides "one" with no call; one whose patterns are other files does not.
/// A run started by the small path is not mined, and a re-split of a
/// template-decided contract counts against the template.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_matching_one_step_template_decides_one_and_a_resplit_demotes_it() {
    use kgraf_orchestrator::templates::{self, TemplateStep};
    let instruction = "BIG part WRITE src/a.rs, then tidy src/b.rs and src/c.rs";
    let seed = |dir: &std::path::Path, paths: &[&str]| {
        let g = kgraf_graph::GraphStore::open(&dir.join("graph.db"), dir).expect("graph");
        let t = templates::from_steps(
            instruction,
            vec![TemplateStep {
                instruction: "tidy the files".into(),
                acceptance: "ok".into(),
                writes: paths.iter().map(|p| templates::write_pattern(p)).collect(),
                depends_on: Vec::new(),
                tools: Vec::new(),
            }],
        )
        .expect("template");
        templates::record_success(&g, &t, "earlier", 1).expect("store");
        t.id
    };
    // Matching: no triage call; the BIG worker hands back and its contract
    // is re-split, which demotes the template.
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let id = seed(dir.path(), &["src/a.rs", "src/b.rs", "src/c.rs"]);
    let p = Parts::open(
        dir.path(),
        |_| "not a plan".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut cfg = auto(false);
    cfg.max_split_depth = 0;
    let report = Orchestrator::new(cfg, p.services(10_000))
        .expect("orchestrator")
        .start(RunId::new("r1"), instruction.into())
        .await
        .expect("run");
    assert_eq!(kind(&p, "triage"), 0, "{:?}", planner_kinds(&p));
    assert_eq!(kind(&p, "decompose"), 0);
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let trace = trace_of(&p).await;
    assert!(triage_steps(&trace).contains(&"template".to_owned()));
    let templates_events: Vec<(String, Vec<String>)> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Templates {
                action, templates, ..
            } => Some((action.clone(), templates.clone())),
            _ => None,
        })
        .collect();
    assert!(
        templates_events.contains(&("failed_on_reuse".to_owned(), vec![id.clone()])),
        "{templates_events:?}"
    );
    assert!(
        !templates_events.iter().any(|(a, _)| a == "mined"),
        "{templates_events:?}"
    );
    // Not matching: the triage call is made.
    let dir2 = tempfile::tempdir().expect("dir");
    with_src(dir2.path());
    seed(dir2.path(), &["lib/x.py"]);
    let p2 = Parts::open(
        dir2.path(),
        |prompt| {
            if prompt.starts_with(TRIAGE) {
                r#"{"size":"one","writes":["src/a.rs"]}"#.into()
            } else {
                "not a plan".into()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    Orchestrator::new(auto(false), p2.services(WINDOW))
        .expect("orchestrator")
        .start(
            RunId::new("r1"),
            "WRITE src/a.rs, then tidy src/b.rs and src/c.rs".into(),
        )
        .await
        .expect("run");
    assert_eq!(kind(&p2, "triage"), 1);
}

/// A gate runner with a scripted verdict per run, a delay, and a record of
/// how many runs overlapped.
struct SlowGate {
    script: Vec<bool>,
    delay: Duration,
    runs: std::sync::Mutex<Vec<GateRequest>>,
    live: AtomicUsize,
    max_live: AtomicUsize,
}

impl SlowGate {
    fn new(script: &[bool], delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            script: script.to_vec(),
            delay,
            runs: std::sync::Mutex::new(Vec::new()),
            live: AtomicUsize::new(0),
            max_live: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl GateRunner for SlowGate {
    fn commands(&self) -> Vec<String> {
        vec!["cargo test --workspace".into()]
    }

    async fn run(&self, request: GateRequest) -> Result<GateVerdict, String> {
        let n = {
            let mut runs = self.runs.lock().expect("lock");
            runs.push(request);
            runs.len() - 1
        };
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(live, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.live.fetch_sub(1, Ordering::SeqCst);
        let passed = self.script[n.min(self.script.len() - 1)];
        Ok(GateVerdict {
            passed,
            by: "cargo test --workspace".into(),
            output: UntrustedText::new(if passed { "ok" } else { "1 test failed" }),
        })
    }
}

/// The small path's root gate runs in the background on the verified tip:
/// one run at a time, a failure after the merge is shown (a notice and a
/// trace notice) and opens a remediation, the next run pins the newest tip,
/// and `Completed` follows its pass. B10 and B10a are measured.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_background_root_gate_shows_a_later_failure_and_completes_after_its_pass() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        |_| "not a plan".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let gate = SlowGate::new(&[false, true], Duration::from_millis(150));
    // A branch whose tip moves by one commit per merge, so each gate run
    // pins the tip it ran on.
    let mut services = p.services(WINDOW);
    services.workspaces = FakeIntegration::new(dir.path().join("worktrees"));
    let report = Orchestrator::new(auto(false), services)
        .expect("orchestrator")
        .with_gate_runner(gate.clone())
        .start(RunId::new("r1"), "Fix the greeting WRITE src/a.rs".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(gate.max_live.load(Ordering::SeqCst), 1, "one run at a time");
    let runs = gate.runs.lock().expect("lock").clone();
    assert_eq!(runs.len(), 2, "{runs:?}");
    let pins: Vec<Option<String>> = runs
        .iter()
        .map(|r| r.pinned.as_ref().map(|c| c.sha.clone()))
        .collect();
    assert_ne!(
        pins[0], pins[1],
        "the rerun is on the remediation's newer tip"
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("background gate gate:root failed")),
        "{:?}",
        report.notices
    );
    let trace = trace_of(&p).await;
    let kinds = latency_kinds(&trace);
    for k in ["contract_merged", "root_gate"] {
        assert!(kinds.contains(&k.to_owned()), "{k}: {kinds:?}");
    }
    let record = record_of(&p);
    assert!(
        record
            .coordination
            .background_gates
            .contains_key("gate:root"),
        "the small path's root gate is a background gate"
    );
}

/// A multi-contract plan's gates are not background gates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_plan_keeps_its_gates_as_they_were() {
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
    let gate = SlowGate::new(&[true], Duration::from_millis(5));
    let report = Orchestrator::new(auto(false), p.services(WINDOW))
        .expect("orchestrator")
        .with_gate_runner(gate.clone())
        .start(
            RunId::new("r1"),
            "Make a and b. Then check them. Then report.".into(),
        )
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(gate.runs.lock().expect("lock").len(), 1);
    let record = record_of(&p);
    assert!(record.coordination.background_gates.is_empty());
    assert_eq!(
        record.triage.map(|t| t.size),
        Some(kgraf_orchestrator::triage::Size::Large)
    );
}

/// Decision 0100's zero-test rule on the small path: a contract whose
/// narrowed check selected no test is unverified, and the mark is cleared
/// once the root gate passes on a tip that holds its merge. With no gate,
/// the mark stays and the run says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_test_mark_is_cleared_when_the_root_gate_passes_over_the_merge() {
    const MARKED: &str =
        "cargo test -p a [scoped_check_vacuous: package a has no recognized test target]";
    async fn run(gate: bool) -> (kgraf_orchestrator::RunReport, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("dir");
        with_src(dir.path());
        let p = Parts::open(
            dir.path(),
            |_| "not a plan".into(),
            |_| SERVES.into(),
            Duration::from_millis(5),
        );
        *p.verifier.by.lock().expect("lock") = Some(MARKED.into());
        let mut services = p.services(WINDOW);
        services.workspaces = FakeIntegration::new(dir.path().join("worktrees"));
        let mut o = Orchestrator::new(auto(false), services).expect("orchestrator");
        if gate {
            o = o.with_gate_runner(SlowGate::new(&[true], Duration::from_millis(5)));
        }
        let report = bounded(o.start(RunId::new("r1"), "Fix the greeting WRITE src/a.rs".into()))
            .await
            .expect("run");
        (report, dir)
    }
    let (gated, _d1) = run(true).await;
    assert_eq!(gated.status, RunStatus::Completed, "{gated:?}");
    assert!(gated.unverified.is_empty(), "{:?}", gated.unverified);
    assert!(
        gated
            .notices
            .iter()
            .any(|n| n.contains("no longer unverified")),
        "{:?}",
        gated.notices
    );
    let (ungated, _d2) = run(false).await;
    assert_eq!(ungated.unverified.len(), 1, "{ungated:?}");
    assert!(
        ungated.unverified[0]
            .1
            .starts_with("scoped_check_vacuous: ")
    );
}

/// A repository-scope early start needs no live contract anywhere: a lease
/// another run holds counts, not only this run's own contracts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_runs_live_lease_stops_a_repository_scope_early_start() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let plan = plan_json(&[c("p1", "make p", "src/p.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.starts_with(TRIAGE) {
                r#"{"size":"split"}"#.into()
            } else {
                plan.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    {
        let g = p.graph.lock().expect("lock");
        kgraf_graph::LeaseRegistry::new(&g)
            .acquire_all(
                "other/x@1",
                "other/x",
                &["lib/other.rs".to_owned()],
                &[None],
                now_ms(),
            )
            .expect("lease");
    }
    let report = bounded(
        Orchestrator::new(auto(true), p.services(WINDOW))
            .expect("orchestrator")
            .start(RunId::new("r1"), "Fix the parser".into()),
    )
    .await
    .expect("run");
    assert_eq!(report.verified, vec!["p1".to_owned()]);
    let unmet = record_of(&p).triage.expect("triage").early_unmet;
    assert!(
        unmet.iter().any(|u| u.contains("another contract is live")),
        "{unmet:?}"
    );
}

/// A stop while an early worker's merge waits for the triage answer never
/// merges it: the worker is cancelled with its worktree and leases released,
/// and the run stops resumable, to be planned in full.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_during_the_merge_hold_cancels_the_early_worker_and_merges_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        |_| "never answered".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    *p.planner.hang_on.lock().expect("lock") = Some(TRIAGE.into());
    let o = Orchestrator::new(auto(true), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(
        RunId::new("r1"),
        "WRITE src/a.rs, then tidy src/b.rs and src/c.rs".into(),
    ));
    until("the early worker was verified", || {
        p.verifier.started.load(Ordering::SeqCst) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.stop().await;
    let report = bounded(run).await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Stopped, "{report:?}");
    assert!(report.verified.is_empty(), "{report:?}");
    let trace = trace_of(&p).await;
    let steps = triage_steps(&trace);
    assert!(steps.contains(&"merge_held".to_owned()), "{steps:?}");
    assert!(steps.contains(&"early_cancelled".to_owned()), "{steps:?}");
    assert!(
        !trace.iter().any(
            |e| matches!(e, TraceEvent::Integration { contract, outcome, .. }
            if contract == "c1" && outcome == "merged")
        ),
        "the early contract merged without an answer"
    );
    let record = record_of(&p);
    assert!(record.open_contracts.is_empty());
    assert!(
        record.admitted_leases.is_empty(),
        "{:?}",
        record.admitted_leases
    );
    assert!(!record.planned, "a resume plans it in full");
}
