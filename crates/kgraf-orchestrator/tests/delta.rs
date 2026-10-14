//! Between-run edits through the real loop (decision 0100, sections 3, 5 and
//! 7): a completed run reopened with a change costs one `replan_delta` call,
//! no `decompose` and no re-grounding; plain resume still refuses a
//! completed run; a kill in each change state resumes with no repeated call
//! and no partial application; a stopped run's running contract is
//! redirected or stopped; a reopen another process races is refused with
//! nothing written; plan review's Change is a delta; a copy keeps the work
//! already in its base and makes one call; the permission table asks
//! through the real path; re-grounding reads the instruction log.

mod support;

use kgraf_core::{AgentId, RunId};
use kgraf_orchestrator::delta::{ChangeSource, ChangeState, RecordedChange};
use kgraf_orchestrator::{
    ChangeInput, ClosedContract, Closure, ContractStatus, Integration, Orchestrator,
    OrchestratorConfig, OrchestratorError, Received, RunStateRecord, RunStatus, RunStore,
    TraceEvent, TraceLog,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;
const CHANGE_MARK: &str = "<change>";

fn cfg() -> OrchestratorConfig {
    let mut c = config();
    c.lease_ttl_ms = 300;
    c
}

async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .expect("finished within 60 s")
}

fn with_src(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("src")).expect("src");
}

/// The planner: a delta prompt gets `delta`, any other prompt `plan`.
fn planner(plan: String, delta: String) -> impl Fn(&str) -> String + Send + Sync + 'static {
    move |prompt: &str| {
        if prompt.contains(CHANGE_MARK) {
            delta.clone()
        } else {
            plan.clone()
        }
    }
}

/// Orchestrator ledger calls by kind.
fn kinds(p: &Parts) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for call in p.ledger.calls().expect("calls") {
        if call.agent_id.starts_with("orchestrator:") {
            *out.entry(call.trace.kind.unwrap_or_default()).or_default() += 1;
        }
    }
    out
}

fn kind(p: &Parts, k: &str) -> usize {
    kinds(p).get(k).copied().unwrap_or(0)
}

fn record_of(p: &Parts, run: &str) -> RunStateRecord {
    RunStore::new(p.graph.clone())
        .load(&RunId::new(run))
        .expect("load")
        .expect("record")
}

fn save(p: &Parts, r: &RunStateRecord) {
    RunStore::new(p.graph.clone()).save(r).expect("save");
}

async fn trace_of(p: &Parts, run: &str) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join(run).join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

fn add_json(id: &str, path: &str, deps: &[&str]) -> String {
    serde_json::json!({
        "kind": "delta",
        "add": [{
            "id": id,
            "instruction": format!("make {id} WRITE {path}"),
            "acceptance": format!("{path} exists"),
            "writes": [{"path": path}],
            "depends_on": deps,
            "tools": ["write_file"],
        }],
    })
    .to_string()
}

fn change_json(id: &str, instruction: &str, path: &str) -> String {
    serde_json::json!({
        "kind": "delta",
        "change": [{
            "id": id,
            "spec": {
                "id": id,
                "instruction": instruction,
                "acceptance": format!("{path} exists"),
                "writes": [{"path": path}],
                "tools": ["write_file"],
            },
            "why": "the user asked",
        }],
    })
    .to_string()
}

/// A first run of one contract `a`, to completion.
async fn completed_run(p: &Parts) {
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let report = bounded(o.start(RunId::new("r1"), "make a".into()))
        .await
        .expect("first run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
}

fn received() -> Received {
    Received::now()
}

/// Acceptance 3: `--resume RUN --change TEXT` on a completed run gives one
/// `replan_delta` row, no `decompose` and no re-grounding, and runs only
/// what the change added. The trace has the `reopened` event and the B4 and
/// B9 latency events, each summing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_run_reopens_with_one_delta_call_and_no_decompose_or_regrounding() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open_with(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            add_json("b", "src/b.rs", &["a"]),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    completed_run(&p).await;
    let decomposed = kind(&p, "decompose");
    let grounded = p.grounder.calls();
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let report = bounded(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("also make b", received()),
    ))
    .await
    .expect("reopen");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(kind(&p, "replan_delta"), 1);
    assert_eq!(kind(&p, "decompose"), decomposed, "no second decompose");
    assert_eq!(
        p.grounder.calls(),
        grounded,
        "no re-grounding on the reopen"
    );
    let r = record_of(&p, "r1");
    assert!(r.is_verified("d1.b"), "{:?}", r.verified_contracts);
    assert_eq!(r.changes.len(), 1);
    assert_eq!(r.changes[0].state, ChangeState::Applied);
    assert_eq!(r.reopens, 1);
    assert!(r.notes.is_empty(), "a change never enters notes");
    assert_eq!(report.changes.len(), 1);
    assert!(report.decompose_tokens.is_some_and(|t| t > 0));
    assert!(report.changes[0].delta_tokens > 0);
    let trace = trace_of(&p, "r1").await;
    assert!(trace.iter().any(|e| matches!(
        e,
        TraceEvent::Reopened {
            session: 1,
            change: 1,
            ..
        }
    )));
    for want in ["delta_applied", "reopen"] {
        let events: Vec<_> = trace
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Latency(l) if serde_json::to_value(l.kind).expect("json") == want => {
                    Some(l.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(events.len(), 1, "{want}");
        assert!(events[0].sums(), "{want}: {:?}", events[0]);
    }
}

/// Plain `--resume` behaves as before: a completed run is refused, and no
/// call is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_resume_still_refuses_a_completed_run() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            String::new(),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    completed_run(&p).await;
    let calls = p.planner.calls();
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let got = o.resume(RunId::new("r1")).await;
    assert!(
        matches!(got, Err(OrchestratorError::AlreadyCompleted(_))),
        "{got:?}"
    );
    assert_eq!(p.planner.calls(), calls);
}

/// A run record of one verified contract `a`, Running, with one change in
/// `state` (an answer saved when the state needs one).
fn seeded(state: ChangeState) -> RunStateRecord {
    let mut r = RunStateRecord::new(RunId::new("r1"), "make a", 0);
    let spec = kgraf_orchestrator::ContractSpec {
        id: "a".into(),
        instruction: "make a WRITE src/a.rs".into(),
        acceptance: "src/a.rs exists".into(),
        writes: vec![kgraf_orchestrator::WriteTarget {
            path: "src/a.rs".into(),
            node: None,
        }],
        depends_on: Vec::new(),
        slices: Vec::new(),
        tools: vec!["write_file".into()],
        route: None,
        attempts: 1,
        task_kind: None,
        check: None,
    };
    let open = kgraf_orchestrator::OpenContract::planned(
        "a",
        "r1/a".into(),
        spec.instruction.clone(),
        spec.acceptance.clone(),
    );
    r.coordination
        .closed
        .push(ClosedContract::of(&open, Closure::Verified));
    r.verified_contracts.push("a".into());
    r.plan_specs.insert("a".into(), spec);
    r.planned = true;
    r.reopens = 1;
    let mut change = RecordedChange::received(1, ChangeSource::Change, "also make b", None, 1, 1);
    change.snapshot = kgraf_orchestrator::delta::snapshot(&r);
    if matches!(
        state,
        ChangeState::Answered
            | ChangeState::Applied
            | ChangeState::Refused { .. }
            | ChangeState::Pending { .. }
    ) {
        change.answer = Some(
            kgraf_orchestrator::delta::parse_delta(&add_json("b", "src/b.rs", &["a"]))
                .expect("answer"),
        );
    }
    // An applied change in a seeded record finished its outside work.
    change.outside_done = state == ChangeState::Applied;
    change.state = state;
    r.changes.push(change);
    r
}

/// Acceptance 6: a kill in each change state resumes with no repeated model
/// call and no partial application. `calls` is the delta calls the resume
/// makes; `applies` whether `d1.b` is added (exactly once).
fn kill_state_case(state: ChangeState, calls: usize, applies: bool) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let dir = tempfile::tempdir().expect("dir");
        with_src(dir.path());
        let p = Parts::open(
            dir.path(),
            planner(String::new(), add_json("b", "src/b.rs", &["a"])),
            |_| SERVES.into(),
            Duration::from_millis(5),
        );
        let name = state.name();
        save(&p, &seeded(state));
        let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
        let report = bounded(o.resume(RunId::new("r1"))).await.expect("resume");
        assert_eq!(kind(&p, "replan_delta"), calls, "{name}: delta calls");
        assert_eq!(kind(&p, "decompose"), 0, "{name}");
        let r = record_of(&p, "r1");
        let adds = r
            .verified_contracts
            .iter()
            .chain(r.open_contracts.iter().map(|c| &c.contract))
            .filter(|c| *c == "d1.b")
            .count();
        assert_eq!(adds, usize::from(applies), "{name}: {report:?}");
        assert_eq!(r.changes.len(), 1, "{name}: no second change entry");
        if applies {
            assert_eq!(r.changes[0].state, ChangeState::Applied, "{name}");
            assert!(r.is_verified("d1.b"), "{name}");
        }
    });
}

#[test]
fn a_kill_after_receipt_makes_the_call_once() {
    kill_state_case(ChangeState::Received, 1, true);
}

#[test]
fn a_kill_during_the_call_makes_the_call_once_more() {
    kill_state_case(ChangeState::Planning, 1, true);
}

#[test]
fn a_kill_after_the_answer_applies_it_with_no_call() {
    kill_state_case(ChangeState::Answered, 0, true);
}

#[test]
fn a_kill_after_application_neither_calls_nor_applies_again() {
    // The record says applied but its op never reached the contracts: a
    // resume trusts the state and applies nothing a second time.
    kill_state_case(ChangeState::Applied, 0, false);
}

#[test]
fn a_refused_or_pending_change_is_left_as_it_is() {
    kill_state_case(
        ChangeState::Refused {
            reason: "two refusals".into(),
        },
        0,
        false,
    );
    kill_state_case(
        ChangeState::Pending {
            reason: "the mode asks".into(),
        },
        0,
        false,
    );
}

/// A workspace provider that records each release.
struct Tracking {
    inner: kgraf_orchestrator::DirWorkspaces,
    released: Mutex<Vec<(std::path::PathBuf, bool)>>,
    /// Workers whose stopped attempt was discarded, in order.
    discarded: Mutex<Vec<String>>,
    /// Workers a `prepare` was asked for.
    prepared: Mutex<Vec<String>>,
}

impl Tracking {
    fn new(p: &Parts) -> Arc<Self> {
        Arc::new(Self {
            inner: kgraf_orchestrator::DirWorkspaces {
                base: p.dir.join("worktrees"),
            },
            released: Mutex::new(Vec::new()),
            discarded: Mutex::new(Vec::new()),
            prepared: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl kgraf_orchestrator::Workspaces for Tracking {
    async fn prepare(
        &self,
        contract: &kgraf_orchestrator::OpenContract,
        worker: &AgentId,
    ) -> Result<std::path::PathBuf, kgraf_orchestrator::PrepareFailure> {
        self.prepared.lock().expect("lock").push(worker.to_string());
        self.inner.prepare(contract, worker).await
    }

    async fn release(&self, workspace: &std::path::Path, keep: bool) {
        self.released
            .lock()
            .expect("lock")
            .push((workspace.to_path_buf(), keep));
    }

    async fn discard_stopped(
        &self,
        _contract: &kgraf_orchestrator::OpenContract,
        worker: &AgentId,
    ) {
        self.discarded
            .lock()
            .expect("lock")
            .push(worker.to_string());
    }
}

/// A stopped run whose contract `c` was running (worker `w1` with a
/// resumable attempt).
fn stopped_with_running(p: &Parts) -> RunStateRecord {
    let mut r = RunStateRecord::new(RunId::new("r1"), "make c", 0);
    let mut c = kgraf_orchestrator::OpenContract::planned(
        "c",
        "r1/c".into(),
        "make c WRITE src/c.rs",
        "src/c.rs exists",
    );
    c.writes = vec![kgraf_orchestrator::WriteTarget {
        path: "src/c.rs".into(),
        node: None,
    }];
    c.tools = vec!["write_file".into()];
    c.status = ContractStatus::Dispatched;
    c.workers = vec![AgentId::new("w1")];
    c.worker_routes = vec!["alpha".into()];
    r.open_contracts.push(c);
    r.planned = true;
    r.status = RunStatus::Stopped;
    save(p, &r);
    r
}

/// Acceptance 4: an amendment of a stopped run's running contract waits as
/// a redirect in the worker's inbox, delivered when it resumes, and is kept
/// on the contract for a later attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_amendment_of_running_work_is_a_redirect_in_its_inbox() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            String::new(),
            change_json("c", "make c WRITE src/c.rs and log it", "src/c.rs"),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    stopped_with_running(&p);
    let hold = p.hold_workers("make c");
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.reopen(RunId::new("r1"), ChangeInput::change("log it", received())));
    // The redirect is written once the applied change is saved.
    until("the redirect is in w1's inbox", || {
        p.graph
            .lock()
            .ok()
            .and_then(|g| g.inbox_item("change-1-w1").ok().flatten())
            .is_some()
    })
    .await;
    let item = {
        let g = p.graph.lock().expect("lock");
        g.inbox_item("change-1-w1").expect("read")
    }
    .expect("a redirect in w1's inbox");
    assert_eq!(item.kind, kgraf_core::session::InboxKind::Redirect);
    assert!(item.text.contains("and log it"), "{}", item.text);
    let r = record_of(&p, "r1");
    let c = r.contract("c").expect("still open");
    assert_eq!(c.workers, vec![AgentId::new("w1")], "the worker resumes");
    assert!(c.redirects.iter().any(|t| t.contains("and log it")));
    hold.add_permits(16);
    handle.stop().await;
    let _ = bounded(run).await;
}

/// Acceptance 4: a supersession of running work stops its attempt: the
/// worker is not continued, its worktree is released (retention decides
/// whether it is kept), its session ends cancelled, and the transcript stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_supersession_of_running_work_cancels_its_attempt() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            String::new(),
            change_json("c", "make c WRITE src/d.rs", "src/d.rs"),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    stopped_with_running(&p);
    let transcript = p.dir.join("runs/r1/workers/w1.jsonl");
    std::fs::create_dir_all(transcript.parent().expect("dir")).expect("mkdir");
    std::fs::write(&transcript, "{}\n").expect("transcript");
    let tracking = Tracking::new(&p);
    let mut services = p.services(WINDOW);
    services.workspaces = tracking.clone();
    let control = Arc::new(kgraf_tools::PermissionControl::manual());
    control
        .set_mode(
            kgraf_tools::PermissionMode::OrchestratorDecides,
            kgraf_tools::ModeSource::UserInterface,
        )
        .expect("mode");
    let o = Orchestrator::new(cfg(), services)
        .expect("orchestrator")
        .with_permissions(control);
    let report = bounded(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("write d instead", received()),
    ))
    .await
    .expect("reopen");
    let r = record_of(&p, "r1");
    let old = r
        .coordination
        .closed
        .iter()
        .find(|c| c.contract == "c")
        .expect("closed");
    assert_eq!(old.closure, Closure::Replaced);
    assert!(r.is_verified("d1.c"), "{report:?}");
    assert_eq!(
        r.changes[0].class,
        Some(kgraf_orchestrator::delta::DeltaClass::Cutting)
    );
    // Discarded by its recorded handle, never by a `prepare` that could
    // make a fresh worktree for it.
    assert_eq!(
        *tracking.discarded.lock().expect("lock"),
        vec!["w1".to_owned()]
    );
    assert!(
        !tracking
            .prepared
            .lock()
            .expect("lock")
            .iter()
            .any(|w| w == "w1"),
        "no prepare for the stopped worker"
    );
    let ended = {
        let g = p.graph.lock().expect("lock");
        g.session_lifecycle(&AgentId::new("w1")).expect("read")
    }
    .expect("a session row");
    assert!(format!("{ended:?}").contains("Cancelled"), "{ended:?}");
    assert!(transcript.exists(), "the transcript is kept");
}

/// Acceptance 12 through the real path: in Manual, a change file applies an
/// additive change, and a cutting one is left pending and fails the exit
/// naming why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_manual_a_cutting_change_waits_and_fails_the_exit() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            String::new(),
            change_json("c", "make c WRITE src/d.rs", "src/d.rs"),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    stopped_with_running(&p);
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let report = bounded(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("write d instead", received()),
    ))
    .await
    .expect("reopen");
    let r = record_of(&p, "r1");
    assert!(
        matches!(r.changes[0].state, ChangeState::Pending { .. }),
        "{:?}",
        r.changes[0].state
    );
    assert!(
        r.contract("d1.c").is_none() && !r.is_verified("d1.c"),
        "the plan is unchanged: no new revision"
    );
    // The old plan does not run either: `c` is not dispatched again.
    let c = r.contract("c").expect("c is still open");
    assert_eq!(c.workers, vec![AgentId::new("w1")]);
    assert_eq!(p.stats.requests().len(), 0, "no worker request at all");
    assert_eq!(report.status, RunStatus::Stopped, "{report:?}");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("stopped before dispatching")),
        "{:?}",
        report.notices
    );
    let line = &report.changes[0];
    assert!(line.blocks_exit(report.session), "{line:?}");
    assert!(line.detail.contains("manual"), "{}", line.detail);
}

/// Acceptance 5: a reopen another process holds the run for is refused, and
/// nothing is written: the change is not lost (it stays with the caller).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopen_of_a_run_another_process_holds_is_refused_with_nothing_written() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            add_json("b", "src/b.rs", &[]),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    completed_run(&p).await;
    {
        let g = p.graph.lock().expect("lock");
        let reg = kgraf_graph::LeaseRegistry::new(&g).with_ttl_ms(60_000);
        let got = reg
            .acquire_all("someone-else", "r1", &["run:r1".to_owned()], &[], now_ms())
            .expect("acquire");
        assert!(matches!(got, kgraf_graph::Acquisition::Granted(_)));
    }
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let got = o
        .reopen(
            RunId::new("r1"),
            ChangeInput::change("also make b", received()),
        )
        .await;
    assert!(
        matches!(got, Err(OrchestratorError::RunBusy { .. })),
        "{got:?}"
    );
    let r = record_of(&p, "r1");
    assert!(r.changes.is_empty(), "nothing written");
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(kind(&p, "replan_delta"), 0);
}

/// Acceptance 11: an edit that changes no line makes no call and writes
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_that_changes_no_line_makes_no_call() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            String::new(),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    completed_run(&p).await;
    let calls = p.planner.calls();
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let got = o
        .reopen(
            RunId::new("r1"),
            ChangeInput::edit("make a  \n", None, received()),
        )
        .await;
    assert!(
        matches!(got, Err(OrchestratorError::NothingChanged)),
        "{got:?}"
    );
    assert_eq!(p.planner.calls(), calls);
    assert!(record_of(&p, "r1").changes.is_empty());
}

/// Acceptance 2 and 7: plan review's Change applies a delta to the shown
/// plan, with one `replan_delta` row and no second `decompose`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_review_change_is_one_delta_and_no_second_decompose() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            add_json("b", "src/b.rs", &[]),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let control = Arc::new(kgraf_tools::PermissionControl::manual());
    control.set_plan_review(true, kgraf_tools::ModeSource::UserInterface);
    let o = Orchestrator::new(cfg(), p.services(WINDOW))
        .expect("orchestrator")
        .with_permissions(control)
        .with_question_attendance(kgraf_orchestrator::QuestionAttendance::Box);
    let desk = o.review_desk();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("the plan is shown", || desk.pending_plan().is_some()).await;
    desk.answer_plan(kgraf_orchestrator::PlanAnswer::Change {
        note: "also make b".into(),
    })
    .expect("change");
    until("the changed plan is shown", || {
        desk.pending_plan().is_some_and(|v| v.items.len() == 2)
    })
    .await;
    desk.answer_plan(kgraf_orchestrator::PlanAnswer::Approve)
        .expect("approve");
    let report = bounded(run).await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(kind(&p, "decompose"), 1);
    assert_eq!(kind(&p, "replan_delta"), 1);
    let r = record_of(&p, "r1");
    assert!(
        r.notes.is_empty(),
        "the change is in the change log, not notes"
    );
    assert_eq!(r.changes[0].source, ChangeSource::PlanReview);
    assert!(r.is_verified("d1.b"));
}

/// A plan review nobody answers does not outlive the run's time limit: the
/// run stops with nothing started and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_review_honors_the_runs_time_limit() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            String::new(),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let control = Arc::new(kgraf_tools::PermissionControl::manual());
    control.set_plan_review(true, kgraf_tools::ModeSource::UserInterface);
    let mut config = cfg();
    config.time_limit = Some(Duration::from_millis(600));
    let o = Orchestrator::new(config, p.services(WINDOW))
        .expect("orchestrator")
        .with_permissions(control)
        .with_question_attendance(kgraf_orchestrator::QuestionAttendance::Box);
    let report = bounded(o.start(RunId::new("r1"), "make a".into()))
        .await
        .expect("run");
    assert_ne!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("time limit ran out while the plan waited")),
        "{:?}",
        report.notices
    );
    assert!(!record_of(&p, "r1").is_verified("a"));
}

/// A record from before the change log keeps its `plan-change-*` notes, and
/// the planner still reads them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_plan_change_note_is_still_read() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            String::new(),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut r = RunStateRecord::new(RunId::new("r1"), "make a", 0);
    r.notes.push(kgraf_orchestrator::RecordedNote {
        id: "plan-change-1-0".into(),
        channel: "plan_review".into(),
        received_ms: 0,
        kind: kgraf_orchestrator::InboundKind::Note,
        text: kgraf_orchestrator::UntrustedText::new("use the word OLDNOTE".to_owned()),
        delivered: false,
    });
    save(&p, &r);
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    bounded(o.resume(RunId::new("r1"))).await.expect("resume");
    assert!(p.planner.prompts()[0].contains("OLDNOTE"));
}

/// A provider whose `commit_landed` answers from a set: commits in it are in
/// the new run's base.
struct Landed {
    inner: kgraf_orchestrator::DirWorkspaces,
    landed: Vec<String>,
}

#[async_trait::async_trait]
impl kgraf_orchestrator::Workspaces for Landed {
    async fn prepare(
        &self,
        contract: &kgraf_orchestrator::OpenContract,
        worker: &AgentId,
    ) -> Result<std::path::PathBuf, kgraf_orchestrator::PrepareFailure> {
        self.inner.prepare(contract, worker).await
    }

    async fn release(&self, workspace: &std::path::Path, keep: bool) {
        self.inner.release(workspace, keep).await
    }

    async fn commit_landed(&self, commit: &str) -> Result<bool, String> {
        Ok(self.landed.iter().any(|c| c == commit))
    }
}

/// Acceptance 10: a copy keeps a contract whose commit is an ancestor of the
/// base, or whose merge was empty, as done; one not in the base, an
/// unversioned one and an open one remain; exactly one delta call applies
/// the edit, and no `decompose` is made. The source is not written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_keeps_work_in_its_base_and_makes_one_delta_call() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(String::new(), add_json("e", "src/e.rs", &[])),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut old = RunStateRecord::new(RunId::new("r0"), "make a b c d f", 0);
    for (id, integration) in [
        (
            "a",
            Integration::Merged {
                commit: "aaa".into(),
            },
        ),
        (
            "b",
            Integration::Merged {
                commit: "bbb".into(),
            },
        ),
        ("c", Integration::Empty),
        ("d", Integration::Unversioned),
    ] {
        let path = format!("src/{id}.rs");
        let spec = kgraf_orchestrator::ContractSpec {
            id: id.into(),
            instruction: format!("make {id} WRITE {path}"),
            acceptance: format!("{path} exists"),
            writes: vec![kgraf_orchestrator::WriteTarget { path, node: None }],
            depends_on: Vec::new(),
            slices: Vec::new(),
            tools: vec!["write_file".into()],
            route: None,
            attempts: 1,
            task_kind: None,
            check: None,
        };
        let open = kgraf_orchestrator::OpenContract::planned(
            id,
            format!("r0/{id}").as_str().into(),
            spec.instruction.clone(),
            spec.acceptance.clone(),
        );
        old.coordination
            .closed
            .push(ClosedContract::of(&open, Closure::Verified));
        old.coordination.integration.insert(id.into(), integration);
        old.verified_contracts.push(id.into());
        old.plan_specs.insert(id.into(), spec);
    }
    let mut f = kgraf_orchestrator::OpenContract::planned(
        "f",
        "r0/f".into(),
        "make f WRITE src/f.rs",
        "src/f.rs exists",
    );
    f.writes = vec![kgraf_orchestrator::WriteTarget {
        path: "src/f.rs".into(),
        node: None,
    }];
    f.tools = vec!["write_file".into()];
    old.open_contracts.push(f);
    old.planned = true;
    old.status = RunStatus::Stopped;
    save(&p, &old);
    let mut services = p.services(WINDOW);
    services.workspaces = Arc::new(Landed {
        inner: kgraf_orchestrator::DirWorkspaces {
            base: p.dir.join("worktrees"),
        },
        landed: vec!["aaa".into()],
    });
    let o = Orchestrator::new(cfg(), services).expect("orchestrator");
    let report = bounded(o.start_from_run(
        RunId::new("r2"),
        RunId::new("r0"),
        "make a b c d f\nand e".into(),
        received(),
    ))
    .await
    .expect("copy");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(kind(&p, "replan_delta"), 1);
    assert_eq!(kind(&p, "decompose"), 0);
    let r = record_of(&p, "r2");
    assert_eq!(r.copied_from, Some(RunId::new("r0")));
    assert_eq!(r.plan_root, Some(RunId::new("r0")));
    let mut verified = r.verified_contracts.clone();
    verified.sort();
    assert_eq!(verified, vec!["a", "b", "c", "d", "d1.e", "f"]);
    let tasks: std::collections::BTreeSet<String> = p
        .ledger
        .calls()
        .expect("calls")
        .into_iter()
        .filter(|c| !c.agent_id.starts_with("orchestrator:"))
        .map(|c| c.task_id)
        .collect();
    for id in ["b", "d", "f", "d1.e"] {
        assert!(tasks.contains(&format!("r2/{id}")), "{id} ran: {tasks:?}");
    }
    for id in ["a", "c"] {
        assert!(
            !tasks.contains(&format!("r2/{id}")),
            "{id} is in the base: {tasks:?}"
        );
    }
    assert_eq!(
        record_of(&p, "r0").status,
        RunStatus::Stopped,
        "the source is not written"
    );
    assert_eq!(r.changes[0].source, ChangeSource::Copy);
}

/// Re-grounding reads the original instruction and every applied change,
/// verbatim (decision 0057 as amended by 0100): a resume of a stopped run
/// whose change applied re-grounds on the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn regrounding_reads_the_instruction_log() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(String::new(), String::new()),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut r = seeded(ChangeState::Applied);
    r.changes[0].text = "the CHANGEMARK change".into();
    let mut b = kgraf_orchestrator::OpenContract::planned(
        "b",
        "r1/b".into(),
        "make b WRITE src/b.rs",
        "src/b.rs exists",
    );
    b.writes = vec![kgraf_orchestrator::WriteTarget {
        path: "src/b.rs".into(),
        node: None,
    }];
    b.tools = vec!["write_file".into()];
    r.open_contracts.push(b);
    r.status = RunStatus::Stopped;
    save(&p, &r);
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    bounded(o.resume(RunId::new("r1"))).await.expect("resume");
    let prompts = p.grounder.prompts();
    assert!(!prompts.is_empty(), "a plain resume re-grounds");
    assert!(prompts[0].contains("CHANGEMARK"), "{}", prompts[0]);
}

fn open_journal(interrupted: bool) -> kgraf_orchestrator::land_record::LandingEntry {
    kgraf_orchestrator::land_record::LandingEntry {
        run_branch: "kgraf/r1".into(),
        candidate: "aaaa".into(),
        merge_commit: "bbbb".into(),
        target: "main".into(),
        target_old: "cccc".into(),
        gate: kgraf_orchestrator::land_record::LandGate::Untested {
            gate: "gate:root".into(),
            reason: "never ran".into(),
        },
        started_ms: 1,
        target_moved: true,
        checkout_interrupted: interrupted,
        include_changes: Vec::new(),
        programs_digest: None,
        fold: None,
        finished_ms: None,
    }
}

/// Acceptance 5 with landing: a reopen while a land journal is open (a land
/// in progress, or a checkout it stopped part way, which is named) is
/// refused with nothing written and no call made; a land holding the run's
/// lease refuses it as busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopen_is_refused_while_a_land_is_open_or_holds_the_run() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            add_json("b", "src/b.rs", &[]),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    completed_run(&p).await;
    for interrupted in [false, true] {
        let mut r = record_of(&p, "r1");
        r.land.landing = Some(open_journal(interrupted));
        save(&p, &r);
        let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
        let got = o
            .reopen(
                RunId::new("r1"),
                ChangeInput::change("also make b", received()),
            )
            .await;
        let Err(OrchestratorError::ReopenRefused { reason, .. }) = &got else {
            panic!("{got:?}");
        };
        if interrupted {
            assert!(
                reason.contains("stopped while git updated your checkout"),
                "{reason}"
            );
        } else {
            assert!(reason.contains("still in progress"), "{reason}");
        }
        assert!(record_of(&p, "r1").changes.is_empty(), "nothing written");
    }
    let mut r = record_of(&p, "r1");
    r.land.landing = None;
    save(&p, &r);
    let _lease = kgraf_orchestrator::take_run_lease_for_land(&p.graph, &RunId::new("r1"), now_ms())
        .expect("the land's lease");
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let got = o
        .reopen(
            RunId::new("r1"),
            ChangeInput::change("also make b", received()),
        )
        .await;
    assert!(
        matches!(got, Err(OrchestratorError::RunBusy { .. })),
        "{got:?}"
    );
    assert!(record_of(&p, "r1").changes.is_empty());
    assert_eq!(kind(&p, "replan_delta"), 0);
}

/// A change card nobody answers does not outlive the run's time limit: the
/// change is pending, named, and the run stops before it dispatches anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_card_honors_the_runs_time_limit() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            String::new(),
            change_json("c", "make c WRITE src/d.rs", "src/d.rs"),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    stopped_with_running(&p);
    let mut config = cfg();
    config.time_limit = Some(Duration::from_millis(600));
    let o = Orchestrator::new(config, p.services(WINDOW))
        .expect("orchestrator")
        .with_question_attendance(kgraf_orchestrator::QuestionAttendance::Box);
    let desk = o.review_desk();
    let report = bounded(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("write d instead", received()),
    ))
    .await
    .expect("reopen");
    assert!(desk.pending_change().is_none(), "the card is withdrawn");
    let r = record_of(&p, "r1");
    assert!(
        matches!(&r.changes[0].state, ChangeState::Pending { reason } if reason.contains("time limit")),
        "{:?}",
        r.changes[0].state
    );
    assert_eq!(report.status, RunStatus::Stopped, "{report:?}");
}

/// The change card (asking follows the permission mode, decision 0100): in
/// Manual, with the user at the box, a cutting change waits on a card naming
/// what it does; yes applies it and the run goes on; no refuses it and the
/// run stops before dispatching the old plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_the_mode_asks_about_waits_on_a_card_and_follows_the_answer() {
    for yes in [true, false] {
        let dir = tempfile::tempdir().expect("dir");
        with_src(dir.path());
        let p = Parts::open(
            dir.path(),
            planner(
                String::new(),
                change_json("c", "make c WRITE src/d.rs", "src/d.rs"),
            ),
            |_| SERVES.into(),
            Duration::from_millis(5),
        );
        stopped_with_running(&p);
        let o = Orchestrator::new(cfg(), p.services(WINDOW))
            .expect("orchestrator")
            .with_question_attendance(kgraf_orchestrator::QuestionAttendance::Box);
        let desk = o.review_desk();
        let run = tokio::spawn(o.reopen(
            RunId::new("r1"),
            ChangeInput::change("write d instead", received()),
        ));
        until("the change card", || desk.pending_change().is_some()).await;
        let card = desk.pending_change().expect("card");
        assert_eq!((card.number, card.class.as_str()), (1, "cutting"));
        assert!(
            card.ops.iter().any(|o| o.contains("replace c with d1.c")),
            "{:?}",
            card.ops
        );
        assert!(matches!(
            desk.answer_change(card.number + 1, yes),
            Err(kgraf_orchestrator::ReviewError::ChangeMoved { .. })
        ));
        assert!(
            desk.pending_change().is_some(),
            "a wrong number answers nothing"
        );
        desk.answer_change(card.number, yes).expect("answer");
        let report = bounded(run).await.expect("join").expect("run");
        let r = record_of(&p, "r1");
        if yes {
            assert_eq!(r.changes[0].state, ChangeState::Applied);
            assert!(r.is_verified("d1.c"), "{report:?}");
            assert_eq!(report.status, RunStatus::Completed);
        } else {
            assert!(matches!(r.changes[0].state, ChangeState::Refused { .. }));
            assert_eq!(report.status, RunStatus::Stopped);
            assert_eq!(p.stats.requests().len(), 0, "the old plan did not run");
        }
    }
}

/// Plan review: a `replan_all` answer with the user at the card is a full
/// re-plan carrying the user's note; the plan is shown again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_replan_answer_at_plan_review_plans_again_with_the_note() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            r#"{"kind":"replan_all","why":"another project"}"#.into(),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let control = Arc::new(kgraf_tools::PermissionControl::manual());
    control.set_plan_review(true, kgraf_tools::ModeSource::UserInterface);
    let o = Orchestrator::new(cfg(), p.services(WINDOW))
        .expect("orchestrator")
        .with_permissions(control)
        .with_question_attendance(kgraf_orchestrator::QuestionAttendance::Box);
    let desk = o.review_desk();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("the plan", || desk.pending_plan().is_some()).await;
    desk.answer_plan(kgraf_orchestrator::PlanAnswer::Change {
        note: "REPLANNOTE please".into(),
    })
    .expect("change");
    until("the second plan", || {
        desk.pending_plan().is_some_and(|v| v.revision == 2)
    })
    .await;
    assert_eq!(kind(&p, "replan_delta"), 1);
    assert_eq!(kind(&p, "decompose"), 2, "a full re-plan");
    assert!(
        p.planner
            .prompts()
            .last()
            .expect("prompt")
            .contains("REPLANNOTE")
    );
    desk.answer_plan(kgraf_orchestrator::PlanAnswer::Approve)
        .expect("approve");
    let report = bounded(run).await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.change_counts.replan_all, 1);
    // The change's measure is against the first plan only: the full
    // re-plan's `decompose` row is not in the denominator.
    let decomposes: Vec<u64> = p
        .ledger
        .calls()
        .expect("calls")
        .into_iter()
        .filter(|c| c.trace.kind.as_deref() == Some("decompose"))
        .map(|c| c.usage.total_input() + c.usage.total_output())
        .collect();
    assert_eq!(decomposes.len(), 2);
    assert_eq!(record_of(&p, "r1").plan_calls.len(), 1);
    assert_eq!(report.decompose_tokens, Some(decomposes[0]));
}

/// A copy judges a copied check as intake does: a check that tests nothing
/// is dropped with a notice even with own checks on; with them off every
/// copied check is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copied_check_is_judged_or_dropped() {
    for own_checks in [true, false] {
        let dir = tempfile::tempdir().expect("dir");
        with_src(dir.path());
        let p = Parts::open(
            dir.path(),
            planner(String::new(), add_json("e", "src/e.rs", &[])),
            |_| SERVES.into(),
            Duration::from_millis(5),
        );
        let mut old = RunStateRecord::new(RunId::new("r0"), "make f", 0);
        let spec = kgraf_orchestrator::ContractSpec {
            id: "f".into(),
            instruction: "make f WRITE src/f.rs".into(),
            acceptance: "src/f.rs exists".into(),
            writes: vec![kgraf_orchestrator::WriteTarget {
                path: "src/f.rs".into(),
                node: None,
            }],
            depends_on: Vec::new(),
            slices: Vec::new(),
            tools: vec!["write_file".into()],
            route: None,
            attempts: 1,
            task_kind: None,
            check: Some(
                if own_checks {
                    "test -f src/f.rs"
                } else {
                    "cargo test"
                }
                .into(),
            ),
        };
        let mut f = kgraf_orchestrator::OpenContract::planned(
            "f",
            "r0/f".into(),
            spec.instruction.clone(),
            spec.acceptance.clone(),
        );
        f.writes = spec.writes.clone();
        f.tools = spec.tools.clone();
        f.contract_checks = vec![spec.check.clone().expect("check")];
        old.open_contracts.push(f);
        old.plan_specs.insert("f".into(), spec);
        old.planned = true;
        old.status = RunStatus::Stopped;
        save(&p, &old);
        let config = OrchestratorConfig {
            own_checks,
            ..cfg()
        };
        let o = Orchestrator::new(config, p.services(WINDOW)).expect("orchestrator");
        let report = bounded(o.start_from_run(
            RunId::new("r2"),
            RunId::new("r0"),
            "make f\nand e".into(),
            received(),
        ))
        .await
        .expect("copy");
        let r = record_of(&p, "r2");
        assert_eq!(r.plan_specs["f"].check, None, "own checks {own_checks}");
        assert!(
            report
                .notices
                .iter()
                .any(|n| n.contains("check copied from run r0 is dropped")),
            "{:?}",
            report.notices
        );
    }
}

/// With own checks off, a `check` the planner wrote is not kept in the plan
/// specs (so no later copy or change can run it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_planned_check_is_not_kept_with_own_checks_off() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let plan = serde_json::json!({"contracts": [{
        "id": "a", "instruction": "make a WRITE src/a.rs", "acceptance": "src/a.rs exists",
        "writes": [{"path": "src/a.rs"}], "tools": ["write_file"], "check": "cargo test -p a",
    }]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        planner(plan, String::new()),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    completed_run(&p).await;
    assert_eq!(record_of(&p, "r1").plan_specs["a"].check, None);
}

/// The plan's own calls read the instruction log: a run planned after a
/// change applied plans the original and the change (here: a record whose
/// plan was lost, planned again on resume).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn planning_reads_the_instruction_log() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            String::new(),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut r = RunStateRecord::new(RunId::new("r1"), "make a", 0);
    let mut change = RecordedChange::received(1, ChangeSource::Change, "PLANLOGMARK", None, 1, 0);
    change.state = ChangeState::Applied;
    change.outside_done = true;
    r.changes.push(change);
    save(&p, &r);
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    bounded(o.resume(RunId::new("r1"))).await.expect("resume");
    assert!(p.planner.prompts()[0].contains("PLANLOGMARK"));
}

/// Acceptance 8 of the review: a kill after the change applied and before
/// its outside work leaves a resume to do it, once: the redirect row (kept
/// as it was when it already exists) and the stopped attempt's discard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_outside_work_of_an_applied_change_is_done_once_after_a_kill() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(String::new(), String::new()),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut r = stopped_with_running(&p);
    r.open_contracts[0].redirects = vec!["The plan changed (change 1). REDIRECTMARK".into()];
    let mut gone = kgraf_orchestrator::OpenContract::planned(
        "g",
        "r1/g".into(),
        "make g WRITE src/g.rs",
        "src/g.rs exists",
    );
    gone.workers = vec![AgentId::new("w9")];
    r.coordination
        .closed
        .push(ClosedContract::of(&gone, Closure::Removed));
    let mut change = RecordedChange::received(1, ChangeSource::Change, "change", None, 1, 1);
    change.state = ChangeState::Applied;
    change.applied = Some(kgraf_orchestrator::delta::AppliedOps {
        redirected: vec!["c".into()],
        removed: vec!["g".into()],
        cancelled: vec![("g".into(), vec!["w9".into()])],
        stopped: vec![gone],
        ..Default::default()
    });
    r.changes.push(change);
    r.reopens = 1;
    r.status = RunStatus::Running;
    save(&p, &r);
    // The redirect row was written before the kill: kept as it is.
    {
        let g = p.graph.lock().expect("lock");
        g.session_upsert(&kgraf_core::session::SessionFacts {
            id: AgentId::new("w1"),
            run: RunId::new("r1"),
            kind: kgraf_core::session::SessionKind::Worker,
            parent: None,
            contract: Some("c".into()),
            route: None,
            model: None,
            depth: 1,
            transcript: None,
            worktree: None,
            write_scope: Vec::new(),
            reasoning: kgraf_core::session::ReasoningVisibility::Unknown,
            started_ms: 1,
            addressable: true,
        })
        .expect("session");
        g.inbox_push(&kgraf_core::session::InboxItem {
            id: "change-1-w1".into(),
            session: AgentId::new("w1"),
            source: kgraf_core::session::InboxSource::User,
            kind: kgraf_core::session::InboxKind::Redirect,
            mode: kgraf_core::session::InjectMode::AtBoundary,
            text: "FIRST WRITE".into(),
            received_ms: 1,
            not_before_ms: 1,
            delivered: None,
        })
        .expect("row");
    }
    let tracking = Tracking::new(&p);
    let mut services = p.services(WINDOW);
    services.workspaces = tracking.clone();
    let o = Orchestrator::new(cfg(), services).expect("orchestrator");
    let report = bounded(o.resume(RunId::new("r1"))).await.expect("resume");
    assert!(
        !report
            .notices
            .iter()
            .any(|n| n.contains("could not be recorded")),
        "the existing row is not written again: {:?}",
        report.notices
    );
    assert_eq!(
        *tracking.discarded.lock().expect("lock"),
        vec!["w9".to_owned()]
    );
    let row = {
        let g = p.graph.lock().expect("lock");
        g.inbox_item("change-1-w1").expect("read")
    }
    .expect("row");
    assert_eq!(row.text, "FIRST WRITE", "the row is written once");
    assert!(record_of(&p, "r1").changes[0].outside_done);
}

/// A gate fake for the stale re-check: every other gate passes at once; the
/// stale one records the run's verified contracts as saved when it starts,
/// waits `delay` (forever with `None`), then fails.
struct SlowStale {
    delay: Option<Duration>,
    graph: Arc<Mutex<kgraf_graph::GraphStore>>,
    verified_at_start: Mutex<Vec<Vec<String>>>,
    started: tokio::sync::Notify,
}

impl SlowStale {
    fn new(p: &Parts, delay: Option<Duration>) -> Arc<Self> {
        Arc::new(Self {
            delay,
            graph: p.graph.clone(),
            verified_at_start: Mutex::new(Vec::new()),
            started: tokio::sync::Notify::new(),
        })
    }
}

#[async_trait::async_trait]
impl kgraf_orchestrator::GateRunner for SlowStale {
    fn commands(&self) -> Vec<String> {
        vec!["cargo test --workspace".into()]
    }
    async fn run(
        &self,
        request: kgraf_orchestrator::GateRequest,
    ) -> Result<kgraf_orchestrator::GateVerdict, String> {
        let stale = request.gate.starts_with("stale:");
        if stale {
            let verified = RunStore::new(self.graph.clone())
                .load(&RunId::new("r1"))
                .ok()
                .flatten()
                .map(|r| r.verified_contracts)
                .unwrap_or_default();
            self.verified_at_start.lock().expect("lock").push(verified);
            self.started.notify_one();
            match self.delay {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        }
        Ok(kgraf_orchestrator::GateVerdict {
            passed: !stale,
            by: "cargo test --workspace".into(),
            output: kgraf_orchestrator::UntrustedText::new("ok"),
        })
    }
}

/// `a` and `b` (depending on `a`) completed; the change reshapes verified
/// `a`, so `b` is stale. Orchestrator decides takes it without a card.
async fn stale_setup(p: &Parts, gate: Arc<SlowStale>) -> Orchestrator {
    let o = Orchestrator::new(cfg(), p.services(WINDOW))
        .expect("orchestrator")
        .with_gate_runner(gate.clone());
    bounded(o.start(RunId::new("r1"), "make a and b".into()))
        .await
        .expect("first run");
    let control = Arc::new(kgraf_tools::PermissionControl::manual());
    control
        .set_mode(
            kgraf_tools::PermissionMode::OrchestratorDecides,
            kgraf_tools::ModeSource::UserInterface,
        )
        .expect("mode");
    Orchestrator::new(cfg(), p.services(WINDOW))
        .expect("orchestrator")
        .with_permissions(control)
        .with_gate_runner(gate)
}

fn stale_parts(dir: &std::path::Path) -> Parts {
    with_src(dir);
    Parts::open(
        dir,
        planner(
            plan_json(&[
                c("a", "make a", "src/a.rs", &[]),
                c("b", "make b", "src/b.rs", &["a"]),
            ]),
            serde_json::json!({"kind": "delta", "change": [{"id": "a", "spec": {
                "id": "a", "instruction": "make a WRITE src/a.rs", "acceptance": "src/a.rs exists",
                "writes": [{"path": "src/a.rs"}, {"path": "src/z.rs"}], "tools": ["write_file"]}}]})
            .to_string(),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    )
}

/// Rule 6: verified work that depends on work a change superseded is
/// checked again in the background once the replacing work merged, on the
/// tip that holds it (not the tip the change applied on); the run waits for
/// a slow re-check; a failure is shown as information and opens no
/// remediation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_verified_work_is_checked_again_and_a_failure_is_shown() {
    let dir = tempfile::tempdir().expect("dir");
    let p = stale_parts(dir.path());
    let gate = SlowStale::new(&p, Some(Duration::from_millis(400)));
    let o = stale_setup(&p, gate.clone()).await;
    let report = bounded(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("a writes z too", received()),
    ))
    .await
    .expect("reopen");
    let r = record_of(&p, "r1");
    let applied = r.changes[0].applied.clone().expect("applied");
    assert_eq!(applied.stale, vec!["b".to_owned()]);
    assert_eq!(applied.stale_after.len(), 1);
    assert!(r.verified_contracts.contains(&applied.stale_after[0]));
    let seen = gate.verified_at_start.lock().expect("lock").clone();
    assert_eq!(seen.len(), 1, "one re-check");
    assert!(
        seen[0].contains(&applied.stale_after[0]),
        "started once the replacing work merged: {seen:?}"
    );
    let trace = trace_of(&p, "r1").await;
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEvent::Change { step, .. } if step == "stale_failed")),
        "the run waited for the slow re-check: {report:?}"
    );
    assert!(
        r.coordination
            .gates
            .iter()
            .all(|g| g.remediations.is_empty()),
        "information only: no remediation"
    );
}

/// A stale re-check still running when the run ends is stopped and said so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_recheck_cut_by_the_runs_end_is_said() {
    let dir = tempfile::tempdir().expect("dir");
    let p = stale_parts(dir.path());
    let gate = SlowStale::new(&p, None);
    let o = stale_setup(&p, gate.clone()).await;
    let handle = o.handle();
    let run = tokio::spawn(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("a writes z too", received()),
    ));
    bounded(gate.started.notified()).await;
    handle.stop().await;
    let report = bounded(run).await.expect("join").expect("reopen");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("did not finish before the run ended")),
        "{:?}",
        report.notices
    );
}

/// The change report counts a failed delta call's tokens, and measures
/// against the first plan's own calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_delta_calls_tokens_are_counted_against_the_first_plan() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            "not json at all".into(),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    completed_run(&p).await;
    let first = record_of(&p, "r1");
    assert_eq!(first.plan_calls.len(), kind(&p, "decompose"));
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    let report = bounded(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("also make b", received()),
    ))
    .await
    .expect("reopen");
    let r = record_of(&p, "r1");
    assert!(matches!(r.changes[0].state, ChangeState::Refused { .. }));
    assert!(
        r.changes[0].tokens > 0,
        "the failed call's tokens are counted"
    );
    assert!(report.change_counts.delta_tokens > 0);
    assert!(report.decompose_tokens.is_some_and(|t| t > 0));
}

/// Rule 6: a straight chain among a change's added contracts collapses even
/// though it depends on verified work outside the change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_of_added_contracts_on_verified_work_collapses() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let delta = serde_json::json!({"kind": "delta", "add": [
        {"id": "x", "instruction": "make x WRITE src/x.rs", "acceptance": "src/x.rs exists",
         "writes": [{"path": "src/x.rs"}], "depends_on": ["a"], "tools": ["write_file"]},
        {"id": "y", "instruction": "make y WRITE src/y.rs", "acceptance": "src/y.rs exists",
         "writes": [{"path": "src/y.rs"}], "depends_on": ["x"], "tools": ["write_file"]},
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        planner(plan_json(&[c("a", "make a", "src/a.rs", &[])]), delta),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    completed_run(&p).await;
    // One route: a chain collapses only when its steps run on one route.
    let mut services = p.services(WINDOW);
    services.routes.truncate(1);
    let o = Orchestrator::new(cfg(), services).expect("orchestrator");
    let report = bounded(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("add x then y", received()),
    ))
    .await
    .expect("reopen");
    let r = record_of(&p, "r1");
    let added = r.changes[0].applied.clone().expect("applied").added;
    assert_eq!(added, vec!["d1.x".to_owned()], "{report:?}");
    assert!(r.is_verified("d1.x"));
}

/// The trace of a reopen opens its session with the `reopened` event, which
/// names when the session before ended (the record's last save): every event
/// of the new session follows it, and readers count the stretch between the
/// two as between sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_event_opens_the_session_and_names_when_the_last_one_ended() {
    let dir = tempfile::tempdir().expect("dir");
    with_src(dir.path());
    let p = Parts::open(
        dir.path(),
        planner(
            plan_json(&[c("a", "make a", "src/a.rs", &[])]),
            add_json("b", "src/b.rs", &["a"]),
        ),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    completed_run(&p).await;
    let mut r = record_of(&p, "r1");
    let ended = r.session_end_ms.expect("the session's end is kept");
    // A later save (a land, a decline) moves the record's last save, not the
    // session's end.
    r.updated_ms = ended + 60_000;
    save(&p, &r);
    let before = trace_of(&p, "r1").await.len();
    let o = Orchestrator::new(cfg(), p.services(WINDOW)).expect("orchestrator");
    bounded(o.reopen(
        RunId::new("r1"),
        ChangeInput::change("also make b", received()),
    ))
    .await
    .expect("reopen");
    let trace = trace_of(&p, "r1").await;
    let session: Vec<&TraceEvent> = trace[before..].iter().collect();
    let first_change = session
        .iter()
        .position(|e| matches!(e, TraceEvent::Change { .. }))
        .expect("a change event");
    let reopened = session
        .iter()
        .position(|e| matches!(e, TraceEvent::Reopened { .. }))
        .expect("a reopened event");
    assert!(reopened < first_change, "reopened comes before the change");
    let TraceEvent::Reopened {
        prev_session_end_ms,
        ..
    } = session[reopened]
    else {
        unreachable!()
    };
    assert_eq!(*prev_session_end_ms, Some(ended));
}
