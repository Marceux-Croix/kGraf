//! Interaction APIs against the real orchestrator loop (fake providers, no
//! network): the orchestrator box's channel port, `/compact`, the user's order
//! (0014), a concurrency cap changed while the run works (0017), and the approval
//! desk's allow-always rule.
//!
//! Each test drives the mechanism and observes its effect: a message is
//! answered through the port the box holds; an order changes which contract a
//! worker is sent next; a raised cap lets more workers run at once.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    AskAnswer, AskDesk, CompactError, DeskNotice, DeskRules, InboundKind, Limits, Orchestrator,
    OrderError, RunStatus, RunStore, TraceEvent, TraceLog, Wake, channel_port,
};
use kgraf_runtime::AskResolver;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

fn wake_counter() -> (Wake, Arc<AtomicUsize>) {
    let n = Arc::new(AtomicUsize::new(0));
    let m = n.clone();
    (
        Arc::new(move || {
            m.fetch_add(1, Ordering::SeqCst);
        }),
        n,
    )
}

async fn dispatch_order(p: &Parts) -> Vec<String> {
    let log = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace");
    log.load()
        .await
        .expect("load")
        .into_iter()
        .filter_map(|e| match e {
            TraceEvent::Dispatch { contract, .. } => Some(contract),
            _ => None,
        })
        .collect()
}

/// Waits until `f` holds, polling the handle's status.
async fn until(
    handle: &kgraf_orchestrator::OrchestratorHandle,
    f: impl Fn(&kgraf_orchestrator::StatusReport) -> bool,
) {
    for _ in 0..500 {
        if let Some(s) = handle.status().await
            && f(&s)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the run never reached the awaited state");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_box_port_carries_a_status_question_and_a_note_through_the_real_loop() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(200),
    );
    let (wake, woken) = wake_counter();
    let (port, mut client) = channel_port("box", wake);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    o.attach_port(port);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until(&handle, |s| !s.in_flight.is_empty()).await;

    let q = client
        .send(InboundKind::StatusQuestion, "how far along?", 1)
        .expect("send");
    let n = client
        .send(InboundKind::Note, "prefer small functions", 2)
        .expect("send");
    let mut replies = Vec::new();
    for _ in 0..500 {
        replies.extend(client.drain_replies());
        if replies.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let status = replies
        .iter()
        .find(|r| r.in_reply_to == q)
        .expect("status reply");
    assert!(status.text.starts_with("run r1:"), "{}", status.text);
    assert!(status.text.contains("1 in flight"), "{}", status.text);
    let noted = replies
        .iter()
        .find(|r| r.in_reply_to == n)
        .expect("note reply");
    assert_eq!(noted.text, "noted, as information for the run");
    assert!(
        woken.load(Ordering::SeqCst) >= 2,
        "each reply wakes the box"
    );

    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed);
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert!(
        record
            .notes
            .iter()
            .any(|note| note.id == n && note.channel == "box"),
        "the note is in the record under the box's id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_names_what_it_could_not_compact() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(300),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until(&handle, |s| !s.in_flight.is_empty()).await;
    let worker = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
        .contract("a")
        .expect("open")
        .workers[0]
        .to_string();

    assert_eq!(
        handle.compact(None).await,
        Err(CompactError::NothingToCompact)
    );
    assert_eq!(
        handle.compact(Some(worker.clone())).await,
        Err(CompactError::RuntimeCannotCompact(worker))
    );
    assert_eq!(
        handle.compact(Some("nobody".into())).await,
        Err(CompactError::UnknownWorker("nobody".into()))
    );
    run.await.expect("join").expect("run");
    assert_eq!(
        handle.compact(None).await,
        Err(CompactError::Stopped),
        "a finished run says so"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_users_order_decides_which_contract_is_dispatched_next() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
        c("c", "make c", "src/c.rs", &[]),
        c("d", "make d", "src/d.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(150),
    );
    // One at a time, so the order is observable as the dispatch sequence.
    let p = Parts {
        admission: admission(Limits::new(1, 8).expect("limits")),
        ..p
    };
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "four parts".into()));
    until(&handle, |s| s.in_flight == ["a"]).await;

    assert_eq!(
        handle.set_order(vec!["zz".into()]).await,
        Err(OrderError::Unknown(vec!["zz".into()]))
    );
    assert_eq!(
        handle.set_order(vec!["d".into(), "d".into()]).await,
        Err(OrderError::Duplicate("d".into()))
    );
    handle
        .set_order(vec!["d".into(), "c".into()])
        .await
        .expect("order");

    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed);
    assert_eq!(
        dispatch_order(&p).await,
        ["a", "d", "c", "b"],
        "the pinned contracts go first, in the user's order; the rest follow"
    );
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(record.coordination.user_order, ["d", "c"], "persisted");
    // Verified contracts keep their briefs in the record, for search (0032).
    assert_eq!(
        record.coordination.closed.len(),
        4,
        "{:?}",
        record.coordination.closed
    );
    assert!(
        record.coordination.closed.iter().all(|c| {
            c.closure == kgraf_orchestrator::Closure::Verified
                && c.instruction.starts_with(&format!("make {}", c.contract))
                && !c.workers.is_empty()
        }),
        "{:?}",
        record.coordination.closed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cap_raised_while_the_run_works_admits_more_workers_at_once() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
        c("c", "make c", "src/c.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(200),
    );
    let p = Parts {
        admission: admission(Limits::new(1, 8).expect("limits")),
        ..p
    };
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "three parts".into()));
    until(&handle, |s| s.in_flight.len() == 1).await;
    // The first worker's request is at the provider, alone.
    for _ in 0..200 {
        if p.stats.max_in_flight.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(p.stats.max_in_flight.load(Ordering::SeqCst), 1);
    // What the settings surface does when the user moves the cap.
    p.admission
        .gate()
        .set_limits(Limits::new(3, 8).expect("limits"));
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed);
    assert!(
        p.stats.max_in_flight.load(Ordering::SeqCst) >= 2,
        "the raised cap let the waiting contracts start beside the first"
    );
}

/// The shell tool asks about a command its policy classifies `Prompt`; allow
/// always records the exact rule the CLI's terminal `a` records, and the next
/// identical call is covered by it without asking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_always_records_an_exact_rule_that_covers_the_next_identical_call() {
    use kgraf_session::approval_store::PersistedAllowStore;
    use kgraf_tools::execpolicy::{Decision, ExecPolicy};
    use kgraf_tools::{Exposure, ShellTool, ToolRegistry};

    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let mut registry = ToolRegistry::new();
    let (shell, _hook) = ShellTool::new(ExecPolicy::new(Decision::Prompt));
    registry
        .register_builtin(Arc::new(shell), Exposure::Prefix)
        .expect("register");
    let (wake, _) = wake_counter();
    let desk = Arc::new(AskDesk::new(
        Some(
            DeskRules::open(
                state.path().join("allow-rules.json"),
                workspace.path(),
                "proj",
                Arc::new(registry),
            )
            .expect("rules"),
        ),
        wake,
    ));
    let paused = || -> kgraf_tools::approval::PausedCall {
        serde_json::from_value(serde_json::json!({
            "call": {
                "call_id": "c1",
                "tool": "shell",
                "input": {"command": ["cargo", "fmt"]},
                "worker": "w1",
                "task": "t1"
            },
            "asked_by": "shell",
            "question": "run `cargo fmt`?",
            "next_hook": 1,
            "rewritten": false,
            "deadline_ms": null,
            "chain_digest": "d",
            "seal": "s"
        }))
        .expect("paused")
    };

    let d = desk.clone();
    let first = tokio::spawn(async move { AskResolver::answer(&*d, &paused()).await });
    let mut id = None;
    for _ in 0..200 {
        if let Some(a) = desk.pending().first() {
            id = Some(a.id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    desk.answer(id.expect("an ask"), AskAnswer::AllowAlways)
        .expect("answer");
    assert_eq!(
        first.await.expect("join"),
        kgraf_tools::approval::UserAnswer::Allow
    );
    assert!(
        desk.take_notices()
            .iter()
            .any(|n| matches!(n, DeskNotice::RuleRecorded { tool, .. } if tool == "shell"))
    );
    // The same call again: covered, answered at once, nobody asked.
    let second = AskResolver::answer(&*desk, &paused()).await;
    assert_eq!(second, kgraf_tools::approval::UserAnswer::Allow);
    assert!(desk.pending().is_empty());
    assert!(
        desk.take_notices()
            .iter()
            .any(|n| matches!(n, DeskNotice::Covered { .. }))
    );
    // And the rule is on disk, where the CLI would find it.
    let (reopened, report) =
        PersistedAllowStore::open(state.path().join("allow-rules.json"), workspace.path())
            .expect("reopen");
    assert_eq!(report.rule_count, 1);
    assert!(
        reopened
            .matching_rule("proj", "shell", Some("[\"cargo\",\"fmt\"]"))
            .is_some()
    );
}

struct Factory {
    parts: Parts,
    ready: Result<(), kgraf_orchestrator::ReadinessFailure>,
}

#[async_trait::async_trait]
impl kgraf_orchestrator::OrchestratorFactory for Factory {
    async fn configured(&self) -> Result<(), kgraf_orchestrator::ReadinessFailure> {
        self.ready.clone()
    }
    fn build(&self, _run: &RunId) -> Result<Orchestrator, kgraf_orchestrator::OrchestratorError> {
        Orchestrator::new(config(), self.parts.services(WINDOW))
    }
}

/// What the desktop shell does: a run hosted in-process, driven from a thread that
/// is not an async task, every result arriving as a polled event with a wake.
#[test]
fn an_interactive_run_is_driven_from_a_plain_thread_to_its_end() {
    use kgraf_orchestrator::{InteractiveRun, RunEvent};
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let factory = Arc::new(Factory {
        parts: Parts::open(
            dir.path(),
            move |_| plan.clone(),
            |_| SERVES.into(),
            Duration::from_millis(200),
        ),
        ready: Ok(()),
    });
    let (wake, woken) = wake_counter();
    let mut run = runtime
        .block_on(InteractiveRun::start(
            runtime.handle().clone(),
            factory,
            RunId::new("r1"),
            "make a".into(),
            wake,
        ))
        .expect("started");

    // From this plain thread: a status question and a compact of the orchestrator.
    std::thread::sleep(Duration::from_millis(50));
    let q = run
        .send(InboundKind::StatusQuestion, "how far?", 1)
        .expect("send");
    run.compact(None);
    let mut events = Vec::new();
    for _ in 0..500 {
        events.extend(run.poll());
        if run.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(run.is_finished(), "{events:?}");
    assert!(
        events.iter().any(
            |e| matches!(e, RunEvent::Reply(r) if r.in_reply_to == q && r.text.starts_with("run r1:"))
        ),
        "{events:?}"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::Compacted {
            target: None,
            result: Err(CompactError::NothingToCompact)
        }
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::Finished(Ok(report)) if report.status == RunStatus::Completed
    )));
    assert!(
        woken.load(Ordering::SeqCst) >= 3,
        "the reply, the compact answer and the end each wake"
    );
}

/// Decision 0067: `InteractiveRun`'s session wrappers, each
/// answered as its own `RunEvent`, and the box's synchronous
/// `pending_questions` read.
#[test]
fn an_interactive_run_exposes_the_session_wrappers() {
    use kgraf_orchestrator::{InteractiveRun, RunEvent};
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("dir");
    // Two independent contracts: stopping `a`'s one attempt must not end the
    // whole run (RunStatus::Blocked would drop the handle's channel before
    // `resume_session` below could reach it) as long as `b` is still in
    // flight.
    let plan = plan_json(&[
        c("a", "make a HANG", "src/a.rs", &[]),
        c("b", "make b HANG", "src/b.rs", &[]),
    ]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    parts.alpha.hang.store(true, Ordering::SeqCst);
    parts.beta.hang.store(true, Ordering::SeqCst);
    let alpha = parts.alpha.clone();
    let beta = parts.beta.clone();
    let factory = Arc::new(Factory {
        parts,
        ready: Ok(()),
    });
    let (wake, _) = wake_counter();
    let mut run = runtime
        .block_on(InteractiveRun::start(
            runtime.handle().clone(),
            factory,
            RunId::new("r1"),
            "make a".into(),
            wake,
        ))
        .expect("started");

    while !(alpha.hang_reached.load(Ordering::SeqCst) || beta.hang_reached.load(Ordering::SeqCst)) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let worker = kgraf_core::AgentId::new("r1-a-r0-a0");

    // `send_to_session`.
    run.send_to_session(
        worker.clone(),
        kgraf_core::session::InboxKind::Note,
        kgraf_core::session::InjectMode::AtBoundary,
        "prefer small functions".into(),
    );
    let sent = wait_for(&mut run, |e| matches!(e, RunEvent::SessionSent { .. }));
    assert!(
        matches!(&sent, RunEvent::SessionSent { session, result: Ok(_) } if session == worker.as_str()),
        "{sent:?}"
    );

    // `stop_session`, then `resume_session`.
    run.stop_session(worker.clone());
    let stopped = wait_for(&mut run, |e| matches!(e, RunEvent::SessionStopped { .. }));
    assert!(
        matches!(&stopped, RunEvent::SessionStopped { result: Ok(()), .. }),
        "{stopped:?}"
    );
    run.resume_session(worker.clone());
    let resumed = wait_for(&mut run, |e| matches!(e, RunEvent::SessionResumed { .. }));
    assert!(
        matches!(&resumed, RunEvent::SessionResumed { result: Ok(()), .. }),
        "{resumed:?}"
    );

    // `pending_questions` (a synchronous read: none pending here) and
    // `answer_question` on an id nobody asked, refused, not silent.
    assert!(run.pending_questions().is_empty());
    run.answer_question(999, "an answer to no question".into());
    let answered = wait_for(&mut run, |e| matches!(e, RunEvent::QuestionAnswered { .. }));
    assert!(
        matches!(
            &answered,
            RunEvent::QuestionAnswered {
                question: 999,
                result: Err(_)
            }
        ),
        "{answered:?}"
    );

    run.stop();
    for _ in 0..500 {
        run.poll();
        if run.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(run.is_finished());
}

/// Polls `run` until an event matching `f` arrives, or panics after a bound.
fn wait_for(
    run: &mut kgraf_orchestrator::InteractiveRun,
    f: impl Fn(&kgraf_orchestrator::RunEvent) -> bool,
) -> kgraf_orchestrator::RunEvent {
    for _ in 0..500 {
        for e in run.poll() {
            if f(&e) {
                return e;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no matching event arrived in time");
}

#[test]
fn a_run_whose_routes_are_not_ready_is_refused_before_it_is_built() {
    use kgraf_orchestrator::{HostError, InteractiveRun, ReadinessFailure};
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("dir");
    let factory = Arc::new(Factory {
        parts: Parts::open(
            dir.path(),
            |_| String::new(),
            |_| SERVES.into(),
            Duration::from_millis(1),
        ),
        ready: Err(ReadinessFailure::Transient("provider unreachable".into())),
    });
    let (wake, _) = wake_counter();
    let err = runtime
        .block_on(InteractiveRun::start(
            runtime.handle().clone(),
            factory,
            RunId::new("r1"),
            "make a".into(),
            wake,
        ))
        .expect_err("not ready");
    assert!(
        matches!(err, HostError::NotReady(ref d) if d.contains("unreachable")),
        "{err}"
    );
}

fn shell_desk(state: &std::path::Path, workspace: &std::path::Path) -> Arc<AskDesk> {
    use kgraf_tools::execpolicy::{Decision, ExecPolicy};
    use kgraf_tools::{Exposure, ShellTool, ToolRegistry};
    let mut registry = ToolRegistry::new();
    let (shell, _hook) = ShellTool::new(ExecPolicy::new(Decision::Prompt));
    registry
        .register_builtin(Arc::new(shell), Exposure::Prefix)
        .expect("register");
    let (wake, _) = wake_counter();
    Arc::new(AskDesk::new(
        Some(
            DeskRules::open(
                state.join("allow-rules.json"),
                workspace,
                "proj",
                Arc::new(registry),
            )
            .expect("rules"),
        ),
        wake,
    ))
}

fn paused_cmd(cmd: &[&str]) -> kgraf_tools::approval::PausedCall {
    serde_json::from_value(serde_json::json!({
        "call": {"call_id": "c1", "tool": "shell", "input": {"command": cmd},
                 "worker": "w1", "task": "t1"},
        "asked_by": "shell", "question": "run it?", "next_hook": 1, "rewritten": false,
        "deadline_ms": null, "chain_digest": "d", "seal": "s"
    }))
    .expect("paused")
}

/// A rule added to the file after the desk opened (at the CLI's terminal, or
/// by hand) covers the next ask without restarting anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_added_on_disk_is_seen_at_the_next_ask() {
    use kgraf_session::approval_store::PersistedAllowStore;
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let desk = shell_desk(state.path(), workspace.path());
    {
        let (mut other, _) =
            PersistedAllowStore::open(state.path().join("allow-rules.json"), workspace.path())
                .expect("store");
        other
            .allow_exact("proj", "shell", "[\"cargo\",\"test\"]", "added elsewhere")
            .expect("rule");
    }
    let answer = AskResolver::answer(&*desk, &paused_cmd(&["cargo", "test"])).await;
    assert_eq!(answer, kgraf_tools::approval::UserAnswer::Allow);
    assert!(desk.pending().is_empty(), "nobody was asked");
    assert!(
        desk.take_notices()
            .iter()
            .any(|n| matches!(n, DeskNotice::Covered { note, .. } if note == "added elsewhere"))
    );
}

/// With a real rule store: allow always on an ask whose worker was aborted
/// is refused, and no rule reaches the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_always_for_an_aborted_worker_records_no_rule() {
    use kgraf_session::approval_store::PersistedAllowStore;
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let desk = shell_desk(state.path(), workspace.path());
    let d = desk.clone();
    let worker =
        tokio::spawn(async move { AskResolver::answer(&*d, &paused_cmd(&["cargo", "fmt"])).await });
    let mut id = None;
    for _ in 0..200 {
        if let Some(a) = desk.pending().first() {
            id = Some(a.id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let id = id.expect("an ask");
    worker.abort();
    let _ = worker.await;
    assert!(desk.pending().is_empty(), "no ghost ask");
    assert!(desk.answer(id, AskAnswer::AllowAlways).is_err());
    let (store, report) =
        PersistedAllowStore::open(state.path().join("allow-rules.json"), workspace.path())
            .expect("store");
    assert_eq!(report.rule_count, 0, "{:?}", store.rules());
}

fn paused_in(
    tool: &str,
    input: serde_json::Value,
    task: &str,
) -> kgraf_tools::approval::PausedCall {
    serde_json::from_value(serde_json::json!({
        "call": {"call_id": "c1", "tool": tool, "input": input, "worker": "w1", "task": task},
        "asked_by": "hook", "question": "run it?", "next_hook": 1, "rewritten": false,
        "deadline_ms": null, "chain_digest": "d", "seal": "s"
    }))
    .expect("paused")
}

/// Item 1: a scheduled run's ask, with nobody at the window, is answered at once:
/// denied when no persisted rule covers it (never shown in the box, never waited
/// on), allowed when one does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scheduled_runs_ask_is_answered_unattended_at_once() {
    use kgraf_orchestrator::AskRouter;
    use kgraf_session::approval_store::PersistedAllowStore;
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let desk = shell_desk(state.path(), workspace.path());
    let router = AskRouter::new(desk.clone());
    let t0 = std::time::Instant::now();
    let answer = AskResolver::answer(
        &router,
        &paused_in(
            "shell",
            serde_json::json!({"command": ["git", "push"]}),
            "sched-n-1/a",
        ),
    )
    .await;
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert!(
        matches!(&answer, kgraf_tools::approval::UserAnswer::Deny { reason }
            if reason.contains("Nobody is attached")),
        "{answer:?}"
    );
    assert!(desk.pending().is_empty(), "never shown in the box");
    // A persisted rule covers it: allowed, as before the box existed.
    {
        let (mut store, _) =
            PersistedAllowStore::open(state.path().join("allow-rules.json"), workspace.path())
                .expect("store");
        store
            .allow_exact("proj", "shell", "[\"git\",\"push\"]", "by hand")
            .expect("rule");
    }
    let answer = AskResolver::answer(
        &router,
        &paused_in(
            "shell",
            serde_json::json!({"command": ["git", "push"]}),
            "sched-n-1/a",
        ),
    )
    .await;
    assert_eq!(answer, kgraf_tools::approval::UserAnswer::Allow);
}

/// Item 1: a run started from the box asks in the box; a scheduled run's ask
/// reaches the box only while a person is at the window, with the desk's timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn box_runs_and_attended_scheduled_runs_ask_in_the_box() {
    use kgraf_orchestrator::AskRouter;
    let (wake, _) = wake_counter();
    let desk = Arc::new(AskDesk::new(None, wake).with_timeout(Duration::from_millis(100)));
    let router = Arc::new(AskRouter::new(desk.clone()));
    router.mark_interactive("box-1");
    let r = router.clone();
    let boxed = tokio::spawn(async move {
        AskResolver::answer(&*r, &paused_in("shell", serde_json::json!({}), "box-1/a")).await
    });
    let mut asked = false;
    for _ in 0..100 {
        if !desk.pending().is_empty() {
            asked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(asked, "the box run's ask is shown in the box");
    let answer = boxed.await.expect("join");
    assert!(
        matches!(&answer, kgraf_tools::approval::UserAnswer::Deny { reason }
            if reason.contains("no answer")),
        "unanswered, it times out: {answer:?}"
    );
    // A scheduled run, with a person at the window: shown in the box too.
    router.set_attended_until(Some(std::time::Instant::now() + Duration::from_secs(60)));
    let r = router.clone();
    let sched = tokio::spawn(async move {
        AskResolver::answer(
            &*r,
            &paused_in("shell", serde_json::json!({}), "sched-n-1/a"),
        )
        .await
    });
    let mut asked = false;
    for _ in 0..100 {
        if let Some(a) = desk.pending().first() {
            desk.answer(a.id, AskAnswer::AllowOnce).expect("answer");
            asked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(asked);
    assert_eq!(
        sched.await.expect("join"),
        kgraf_tools::approval::UserAnswer::Allow
    );
}

/// Item 2: allow always on an `mcp__` tool records the any-call rule the CLI's
/// terminal `a` records, and says so; then an unattended call to the same tool
/// (a scheduled run) runs under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_always_on_an_mcp_tool_records_an_any_call_rule() {
    use kgraf_orchestrator::AskRouter;
    use kgraf_session::approval_store::PersistedAllowStore;
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let desk = shell_desk(state.path(), workspace.path());
    let router = Arc::new(AskRouter::new(desk.clone()));
    router.mark_interactive("box-1");
    let tool = "mcp__docs__search";
    let r = router.clone();
    let boxed = tokio::spawn(async move {
        AskResolver::answer(
            &*r,
            &paused_in(tool, serde_json::json!({"query": "x"}), "box-1/a"),
        )
        .await
    });
    let mut id = None;
    for _ in 0..200 {
        if let Some(a) = desk.pending().first() {
            id = Some(a.id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    desk.answer(id.expect("asked"), AskAnswer::AllowAlways)
        .expect("answer");
    assert_eq!(
        boxed.await.expect("join"),
        kgraf_tools::approval::UserAnswer::Allow
    );
    assert!(
        desk.take_notices().iter().any(|n| matches!(n,
            DeskNotice::RuleRecorded { tool: t, subject } if t == tool && subject.contains("any call"))),
        "the box says an any-call rule was recorded"
    );
    let (store, _) =
        PersistedAllowStore::open(state.path().join("allow-rules.json"), workspace.path())
            .expect("store");
    assert!(store.matching_rule("proj", tool, None).is_some(), "on disk");
    // An unattended call to the same tool, from a scheduled run, now runs.
    let answer = AskResolver::answer(
        &*router,
        &paused_in(tool, serde_json::json!({"query": "other"}), "sched-n-2/b"),
    )
    .await;
    assert_eq!(answer, kgraf_tools::approval::UserAnswer::Allow);
}

/// D3: attendance that has lapsed (the last input too long ago) answers a
/// scheduled run's ask unattended, at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_attendance_answers_a_scheduled_ask_unattended() {
    use kgraf_orchestrator::AskRouter;
    let (wake, _) = wake_counter();
    let desk = Arc::new(AskDesk::new(None, wake));
    let router = AskRouter::new(desk.clone());
    // Visible and focused, but the last input was long ago: the window passes a
    // time already past.
    router.set_attended_until(Some(std::time::Instant::now() - Duration::from_secs(1)));
    let t0 = std::time::Instant::now();
    let answer = AskResolver::answer(
        &router,
        &paused_in("shell", serde_json::json!({}), "sched-n-1/a"),
    )
    .await;
    assert!(t0.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        answer,
        kgraf_tools::approval::UserAnswer::Deny { .. }
    ));
    assert!(desk.pending().is_empty());
}

/// D3: a scheduled run's ask waiting in the box is denied the moment attendance
/// turns false, and never waits past the scheduled timeout (60 s by default).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_scheduled_ask_is_denied_when_attendance_lapses_or_times_out() {
    use kgraf_orchestrator::{AskRouter, SCHEDULED_ASK_TIMEOUT};
    assert_eq!(SCHEDULED_ASK_TIMEOUT, Duration::from_secs(60));
    let (wake, _) = wake_counter();
    let desk = Arc::new(AskDesk::new(None, wake));
    let router = Arc::new(AskRouter::new(desk.clone()));
    router.set_attended_until(Some(std::time::Instant::now() + Duration::from_secs(60)));
    let r = router.clone();
    let sched = tokio::spawn(async move {
        AskResolver::answer(
            &*r,
            &paused_in("shell", serde_json::json!({}), "sched-n-1/a"),
        )
        .await
    });
    let mut shown = false;
    for _ in 0..200 {
        if !desk.pending().is_empty() {
            shown = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(shown, "shown while attended");
    let t0 = std::time::Instant::now();
    router.set_attended_until(None);
    let answer = sched.await.expect("join");
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert!(
        matches!(&answer, kgraf_tools::approval::UserAnswer::Deny { reason }
            if reason.contains("Nobody is at the orchestrator box")),
        "{answer:?}"
    );
    assert!(desk.pending().is_empty(), "the ask left the box");

    // The scheduled timeout, shortened for the test.
    let router = AskRouter::new(desk.clone()).with_scheduled_timeout(Duration::from_millis(100));
    router.set_attended_until(Some(std::time::Instant::now() + Duration::from_secs(60)));
    let answer = AskResolver::answer(
        &router,
        &paused_in("shell", serde_json::json!({}), "sched-n-2/a"),
    )
    .await;
    assert!(
        matches!(&answer, kgraf_tools::approval::UserAnswer::Deny { reason }
            if reason.contains("no answer")),
        "{answer:?}"
    );
}
