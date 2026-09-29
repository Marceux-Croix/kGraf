//! The orchestrator host against real runs (M7): a real orchestrator loop
//! built by a factory over the orchestrator crate's own fakes (fake providers that
//! settle real call tokens, a real ledger and graph store), hosted by the shell's
//! `OrchestratorHost` exactly as the window hosts it.
//!
//! Each test drives the mechanism and reads its effect: the cap moved in
//! the shell binds the admission the run uses; the desk answers a real worker's
//! ask; Stop then Play leaves the run resumable and resumes it; closing the window
//! stops the run for real.

#[allow(dead_code, unused_imports, clippy::all)]
#[path = "../../kgraf-orchestrator/tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use kgraf_core::RunId;
use kgraf_orchestrator::{
    AskDesk, Orchestrator, OrchestratorError, OrchestratorFactory, ReadinessFailure, RunStatus,
    RunStore,
};
use kgraf_runtime::AskResolver;
use kgraf_shell::interaction::{Interaction, PlayAction};
use kgraf_shell::orchestrator_box::{BackendState, BoxBackend};
use kgraf_shell::orchestrator_host::{FixedAdmission, HostParts, OrchestratorHost};
use kgraf_tools::approval::{ApprovalChain, ApprovalHook, HookDecision, PendingCall};
use support::*;

const WINDOW: u64 = 200_000;

/// Asks the user about every file write.
struct AskOnWrite;

#[async_trait::async_trait]
impl ApprovalHook for AskOnWrite {
    fn name(&self) -> &str {
        "ask-on-write"
    }
    async fn review(&self, call: &PendingCall) -> HookDecision {
        if call.tool == "write_file" {
            HookDecision::Ask {
                question: "write this file?".into(),
            }
        } else {
            HookDecision::Allow
        }
    }
}

struct Factory {
    parts: Parts,
    desk: Arc<AskDesk>,
    ask_on_write: bool,
    /// A worker's `ask_user_question` is a real tool call,
    /// only reachable when the orchestrator's services register it
    /// (`kgraf_orchestrator::session_tools`) and its `SessionLine` is bound
    /// to the built orchestrator's own handle -- exactly `with_tools` does in
    /// `kgraf-orchestrator/tests/sessions_review.rs`, replicated here so a
    /// worker's own tool call, not `OrchestratorHandle::ask_question`
    /// directly, is what raises the question this file's own test answers.
    session_line: Option<kgraf_orchestrator::SessionLine>,
}

#[async_trait::async_trait]
impl OrchestratorFactory for Factory {
    async fn configured(&self) -> Result<(), ReadinessFailure> {
        Ok(())
    }
    fn build(&self, _run: &RunId) -> Result<Orchestrator, OrchestratorError> {
        let services = match &self.session_line {
            Some(line) => self
                .parts
                .services_with(WINDOW, &kgraf_orchestrator::session_tools(line)),
            None => self.parts.services(WINDOW),
        };
        let mut services = services;
        if self.ask_on_write {
            for route in &mut services.routes {
                let env = Arc::get_mut(&mut route.env).expect("a fresh route env");
                env.approval = ApprovalChain::new(vec![Arc::new(AskOnWrite)]);
                env.asks = Some(self.desk.clone() as Arc<dyn AskResolver>);
            }
        }
        let o = Orchestrator::new(config(), services)?;
        if let Some(line) = &self.session_line {
            line.bind(o.handle()).expect("bind the session line once");
        }
        Ok(o)
    }
}

fn host_parts(parts: Parts, ask_on_write: bool) -> (HostParts, Arc<Factory>) {
    host_parts_inner(parts, ask_on_write, None)
}

/// See `Factory::session_line`'s own doc comment.
fn host_parts_with_session_tools(parts: Parts) -> (HostParts, Arc<Factory>) {
    host_parts_inner(parts, false, Some(kgraf_orchestrator::SessionLine::new()))
}

fn host_parts_inner(
    parts: Parts,
    ask_on_write: bool,
    session_line: Option<kgraf_orchestrator::SessionLine>,
) -> (HostParts, Arc<Factory>) {
    let desk = Arc::new(AskDesk::new(None, Arc::new(|| {})));
    let admission = parts.admission.clone();
    let factory = Arc::new(Factory {
        parts,
        desk: desk.clone(),
        ask_on_write,
        session_line,
    });
    (
        HostParts {
            factory: factory.clone(),
            housekeeping: Arc::new(FixedAdmission(admission)),
            router: None,
            desk,
        },
        factory,
    )
}

/// Polls the host until `done` holds.
fn until(host: &mut OrchestratorHost, what: &str, done: impl Fn(&mut OrchestratorHost) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(host) {
        assert!(Instant::now() < deadline, "never: {what}");
        let _ = BoxBackend::poll(host);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn record(factory: &Factory, run: &str) -> kgraf_orchestrator::RunStateRecord {
    RunStore::new(factory.parts.graph.clone())
        .load(&RunId::new(run))
        .expect("load")
        .expect("record")
}

fn running_run(host: &OrchestratorHost) -> Option<String> {
    host.running().map(|r| r.run().to_string())
}

/// The cap moved in the shell is the cap of the admission the hosted run uses:
/// three independent contracts run one at a time.
#[test]
fn the_cap_set_in_the_shell_binds_the_hosted_runs_admission() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
        c("c", "make c", "src/c.rs", &[]),
    ]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(60),
    );
    let (hp, factory) = host_parts(parts, false);
    let ctx = egui::Context::default();
    let mut interaction = Interaction::new(&ctx, None, Some(hp));
    interaction.sync_parallelism(kgraf_orchestrator::Limits::new(1, 8).expect("limits"));
    assert_eq!(factory.parts.admission.gate().limits().max_work_items(), 1);

    let host = interaction.host_mut();
    host.start("three parts").expect("start");
    until(host, "the run starts", |h| running_run(h).is_some());
    let run = running_run(host).expect("run");
    until(host, "the run ends", |h| h.running().is_none());
    assert_eq!(record(&factory, &run).status, RunStatus::Completed);
    assert_eq!(
        factory
            .parts
            .stats
            .max_in_flight
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the shell's cap of 1 bound the run"
    );
}

/// The desk is the worker's ask resolver in a real run: each write waits in the
/// box until answered there, and the run completes once it is.
#[test]
fn the_desk_answers_a_real_workers_ask() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(10),
    );
    let (hp, factory) = host_parts(parts, true);
    let mut host = OrchestratorHost::new(Some(hp), Arc::new(|| {})).expect("host");
    host.start("make a").expect("start");
    until(&mut host, "the worker asks", |h| {
        !h.pending_asks().is_empty()
    });
    let ask = host.pending_asks()[0].clone();
    assert_eq!(ask.tool, "write_file");
    assert_eq!(ask.question, "write this file?");
    let run = running_run(&host).expect("run");
    BoxBackend::answer(&mut host, ask.id, kgraf_orchestrator::AskAnswer::AllowOnce)
        .expect("answer");
    // Later attempts may ask again; answer each as it comes.
    let deadline = Instant::now() + Duration::from_secs(20);
    while host.running().is_some() {
        assert!(Instant::now() < deadline, "the run never ended");
        for a in host.pending_asks() {
            let _ = BoxBackend::answer(&mut host, a.id, kgraf_orchestrator::AskAnswer::AllowOnce);
        }
        let _ = BoxBackend::poll(&mut host);
        std::thread::sleep(Duration::from_millis(10));
    }
    let r = record(&factory, &run);
    assert_eq!(r.status, RunStatus::Completed, "{:?}", r.status);
    assert_eq!(r.verified_contracts, ["a"]);
}

/// Design 0067: `OrchestratorHost::send_to_session`, `stop_session` and
/// `resume_session` reach a real `InteractiveRun` (a fake provider settling
/// real tokens, per this file's own module doc), not a stub refusal. Each
/// answer arrives later as a `BackendEvent` from `poll`, since
/// `InteractiveRun`'s own methods are fire-and-forget. `answer_question` and
/// `pending_questions` are exercised separately, through a worker's own
/// `ask_user_question` tool call, not this test (item 2):
/// `a_question_raised_through_the_worker_tool_is_answered_through_the_host`,
/// below.
#[test]
fn session_actions_reach_a_real_worker_through_the_host() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(10),
    );
    let (hp, _factory) = host_parts(parts, true);
    let mut host = OrchestratorHost::new(Some(hp), Arc::new(|| {})).expect("host");
    host.start("make a").expect("start");
    until(&mut host, "the worker asks", |h| {
        !h.pending_asks().is_empty()
    });
    let worker = kgraf_core::AgentId::new(host.pending_asks()[0].worker.clone());

    /// Sends `action`, then polls until a `BackendEvent` about `worker`
    /// arrives (never dropped: fail loud): asserts one did, and that it is
    /// not a "no run" refusal (the stub's own wording), so this fails loudly
    /// if a future change silently reintroduces the stub.
    fn drive(
        host: &mut OrchestratorHost,
        action: impl FnOnce(&mut OrchestratorHost) -> Result<(), String>,
        worker: &str,
    ) {
        action(host).expect("the host accepts the action");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "no answer arrived for {worker}");
            for e in BoxBackend::poll(host) {
                let text = match &e {
                    kgraf_shell::orchestrator_box::BackendEvent::Notice(t)
                    | kgraf_shell::orchestrator_box::BackendEvent::Error(t) => t.clone(),
                    kgraf_shell::orchestrator_box::BackendEvent::Reply { .. } => continue,
                };
                if text.contains(worker) {
                    assert!(
                        !text.contains("no run in this window"),
                        "the real host must not fall back to the stub refusal: {text}"
                    );
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    drive(
        &mut host,
        |h| {
            BoxBackend::send_to_session(
                h,
                worker.clone(),
                kgraf_core::session::InboxKind::Note,
                kgraf_core::session::InjectMode::AtBoundary,
                "noted".into(),
            )
        },
        worker.as_str(),
    );
    drive(
        &mut host,
        |h| BoxBackend::stop_session(h, worker.clone()),
        worker.as_str(),
    );
    drive(
        &mut host,
        |h| BoxBackend::resume_session(h, worker.clone()),
        worker.as_str(),
    );
    // Answer whatever the worker is actually waiting on, so the run does not
    // hang forever after being stopped and resumed above.
    for a in host.pending_asks() {
        let _ = BoxBackend::answer(&mut host, a.id, kgraf_orchestrator::AskAnswer::AllowOnce);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while host.running().is_some() {
        assert!(Instant::now() < deadline, "the run never ended");
        for a in host.pending_asks() {
            let _ = BoxBackend::answer(&mut host, a.id, kgraf_orchestrator::AskAnswer::AllowOnce);
        }
        let _ = BoxBackend::poll(&mut host);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `pending_questions` and `answer_question` reach a real
/// worker's `ask_user_question` call (not the write-approval desk this
/// file's other tests use), through the real host.
#[test]
fn a_question_raised_through_the_worker_tool_is_answered_through_the_host() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a ASK", "src/a.rs", &[])]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(10),
    );
    let (hp, factory) = host_parts_with_session_tools(parts);
    let mut host = OrchestratorHost::new(Some(hp), Arc::new(|| {})).expect("host");
    host.start("make a ASK").expect("start");
    let run = running_run(&host)
        .or_else(|| {
            until(&mut host, "the run starts", |h| running_run(h).is_some());
            running_run(&host)
        })
        .expect("run");
    until(&mut host, "the worker's question reaches the box", |h| {
        let _ = BoxBackend::poll(h);
        !BoxBackend::pending_questions(h).is_empty()
    });
    let q = BoxBackend::pending_questions(&host)[0].clone();
    assert!(
        q.question.contains("config file"),
        "the real question text, not a placeholder: {q:?}"
    );
    BoxBackend::answer_question(&mut host, q.id, "kgraf.toml".into()).expect("the host accepts it");
    let deadline = Instant::now() + Duration::from_secs(20);
    while host.running().is_some() {
        assert!(Instant::now() < deadline, "the run never ended");
        for a in host.pending_asks() {
            let _ = BoxBackend::answer(&mut host, a.id, kgraf_orchestrator::AskAnswer::AllowOnce);
        }
        let _ = BoxBackend::poll(&mut host);
        std::thread::sleep(Duration::from_millis(10));
    }
    let r = record(&factory, &run);
    assert_eq!(r.status, RunStatus::Completed, "{:?}", r.status);
}

/// Stop, then Play: the run stops resumable, Play offers exactly it, and the
/// resumed run completes.
#[test]
fn stop_then_play_resumes_the_same_run() {
    let root = tempfile::tempdir().expect("dir");
    let state = root.path().join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let parts = Parts::open(
        &state,
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(250),
    );
    let (hp, factory) = host_parts(parts, false);
    let ctx = egui::Context::default();
    let mut interaction = Interaction::new(&ctx, Some(root.path()), Some(hp));
    interaction.host_mut().start("a then b").expect("start");
    until(interaction.host_mut(), "the run starts", |h| {
        running_run(h).is_some()
    });
    let run = running_run(interaction.host_mut()).expect("run");
    assert_eq!(interaction.play_action(), PlayAction::Stop);
    interaction.play();
    until(interaction.host_mut(), "the run stops", |h| {
        h.running().is_none()
    });
    assert_eq!(record(&factory, &run).status, RunStatus::Stopped);

    // The run watch sees the stopped record; Play offers that run.
    let deadline = Instant::now() + Duration::from_secs(20);
    while interaction.play_action() != PlayAction::Resume(run.clone()) {
        assert!(
            Instant::now() < deadline,
            "Play never offered the stopped run: {:?}",
            interaction.play_action()
        );
        interaction.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    interaction.play();
    until(interaction.host_mut(), "the run resumes", |h| {
        running_run(h).is_some()
    });
    assert_eq!(
        running_run(interaction.host_mut()).as_deref(),
        Some(run.as_str())
    );
    until(interaction.host_mut(), "the resumed run ends", |h| {
        h.running().is_none()
    });
    let r = record(&factory, &run);
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(r.verified_contracts, ["a", "b"]);
}

/// N5: the window's quit starts the stop off the UI thread and returns at once;
/// the receiver answers when the run has stopped, saved as stopped.
#[test]
fn start_shutdown_returns_at_once_and_answers_when_the_run_stops() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(2_000),
    );
    let (hp, factory) = host_parts(parts, false);
    let mut host = OrchestratorHost::new(Some(hp), Arc::new(|| {})).expect("host");
    host.start("make a").expect("start");
    until(&mut host, "the run starts", |h| running_run(h).is_some());
    let run = running_run(&host).expect("run");
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    let rx = host.start_shutdown().expect("a run is hosted");
    assert!(
        t0.elapsed() < Duration::from_millis(100),
        "the UI thread is not blocked: {:?}",
        t0.elapsed()
    );
    assert!(host.running().is_none());
    let ended = rx
        .recv_timeout(kgraf_shell::orchestrator_host::STOP_WAIT)
        .expect("an answer");
    assert!(ended, "the run ended within the bound");
    assert_eq!(record(&factory, &run).status, RunStatus::Stopped);
    assert!(host.start_shutdown().is_none(), "nothing left to stop");
}

/// L4: closing the window (dropping the host) stops a hosted run for real, within
/// the bound: its record is saved as stopped, not left running.
#[test]
fn closing_the_window_stops_the_run() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(2_000),
    );
    let (hp, factory) = host_parts(parts, false);
    let mut host = OrchestratorHost::new(Some(hp), Arc::new(|| {})).expect("host");
    host.start("make a").expect("start");
    until(&mut host, "the run starts", |h| {
        running_run(h).is_some() && matches!(h.state(), BackendState::Running { .. })
    });
    let run = running_run(&host).expect("run");
    // Let the worker get out.
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    drop(host);
    assert!(t0.elapsed() < kgraf_shell::orchestrator_host::STOP_WAIT);
    assert_eq!(record(&factory, &run).status, RunStatus::Stopped);
}

// ------------------------------------------------------------------ switcher (0066)

use kgraf_shell::projects::{ProjectParts, ProjectStatus, Workspace};

/// A project folder whose runs are hosted through the fakes: its store is in
/// `.kgraf`, where the window's run watch reads the records.
fn hosted_project(
    plan: String,
    delay: Duration,
    ask_on_write: bool,
) -> (tempfile::TempDir, HostParts, Arc<Factory>) {
    let root = tempfile::tempdir().expect("dir");
    let state = root.path().join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let parts = Parts::open(&state, move |_| plan.clone(), |_| SERVES.into(), delay);
    let (hp, factory) = host_parts(parts, ask_on_write);
    (root, hp, factory)
}

/// Ticks the workspace, as the window does each frame, until `done` holds.
fn tick_until(
    ctx: &egui::Context,
    w: &mut Workspace,
    what: &str,
    mut done: impl FnMut(&mut Workspace) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(w) {
        assert!(Instant::now() < deadline, "never: {what}");
        w.tick(ctx);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Decision 0066: a run started in project A keeps going while project B is
/// shown, and A's dot follows its real state: gray while it works, cyan once its
/// results are new, none once the user looks at A again. B stays idle.
#[test]
fn a_run_in_one_project_continues_while_another_is_shown() {
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let (a_root, hp, factory) = hosted_project(plan, Duration::from_millis(150), false);
    let b_root = tempfile::tempdir().expect("dir");
    let ctx = egui::Context::default();
    let mut w = Workspace::default();
    let a = w.open(a_root.path().to_path_buf(), || ProjectParts {
        host: Some(hp),
        ..ProjectParts::default()
    });
    let b = w.open(b_root.path().to_path_buf(), ProjectParts::default);
    assert_eq!(
        w.active_index(),
        Some(a),
        "the first project opened is shown"
    );
    w.tick(&ctx);
    assert_eq!(w.statuses(), [ProjectStatus::Idle, ProjectStatus::Idle]);

    // Start a run in A, then show B.
    let host = w.projects_mut()[a]
        .interaction
        .as_mut()
        .expect("interaction")
        .host_mut();
    host.start("a then b").expect("start");
    w.switch_to(b);
    assert_eq!(w.active_index(), Some(b));

    // A works in the background: its dot is gray, and so is the hamburger.
    tick_until(&ctx, &mut w, "A is working", |w| {
        w.projects()[a].status() == ProjectStatus::Working
    });
    assert_eq!(w.hamburger(), ProjectStatus::Working);
    assert_eq!(w.projects()[b].status(), ProjectStatus::Idle);
    // Starting counts as working; wait until the run is up to read its id.
    tick_until(&ctx, &mut w, "A's run is up", |w| {
        w.projects()[a]
            .interaction
            .as_ref()
            .is_some_and(|i| i.host().running().is_some())
    });
    assert_eq!(w.projects()[a].status(), ProjectStatus::Working);
    let run = w.projects()[a]
        .interaction
        .as_ref()
        .and_then(|i| i.host().running().map(|r| r.run().to_string()))
        .expect("A hosts the run");

    // Only the background ticks drive A; B stays shown the whole time.
    tick_until(&ctx, &mut w, "A's run ends", |w| {
        w.projects()[a]
            .interaction
            .as_ref()
            .is_some_and(|i| i.host().running().is_none())
    });
    assert_eq!(w.active_index(), Some(b), "B stayed shown");
    let r = record(&factory, &run);
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(r.verified_contracts, ["a", "b"]);

    // Its results are new to the user: cyan on A's row and on the hamburger.
    tick_until(&ctx, &mut w, "A has new results", |w| {
        w.projects()[a].status() == ProjectStatus::NewResults
    });
    assert_eq!(w.hamburger(), ProjectStatus::NewResults);
    assert!(w.projects()[a].state_line().starts_with("New results"));

    // Looking at A sees them: no dot. The run's admission and lease release
    // a moment after the run reports ended, and "working" outranks idle, so
    // the flag is awaited (an event on the project's own signals, not a
    // wall-clock guess) before the dot is read.
    w.switch_to(a);
    tick_until(&ctx, &mut w, "A's admission and lease are released", |w| {
        !w.projects()[a].flags().working
    });
    assert_eq!(w.projects()[a].status(), ProjectStatus::Idle);
    assert_eq!(w.hamburger(), ProjectStatus::Idle);
}

/// The bell counts pending approvals (plus questions, none of which the runtime
/// can ask yet) over every open project, from the real desks; the project that
/// waits is yellow in its row and in the hamburger, whichever project is shown.
#[test]
fn the_bell_counts_real_pending_approvals_across_projects() {
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let (a_root, hp, _factory) = hosted_project(plan, Duration::from_millis(10), true);
    let b_root = tempfile::tempdir().expect("dir");
    let ctx = egui::Context::default();
    let mut w = Workspace::default();
    let a = w.open(a_root.path().to_path_buf(), || ProjectParts {
        host: Some(hp),
        ..ProjectParts::default()
    });
    let b = w.open(b_root.path().to_path_buf(), ProjectParts::default);
    w.tick(&ctx);
    assert_eq!(w.bell(), 0);
    w.projects_mut()[a]
        .interaction
        .as_mut()
        .expect("interaction")
        .host_mut()
        .start("make a")
        .expect("start");
    w.switch_to(b);
    tick_until(&ctx, &mut w, "the worker asks", |w| w.bell() > 0);
    let pending = w.projects()[a]
        .interaction
        .as_ref()
        .expect("interaction")
        .host()
        .pending_asks()
        .len();
    assert_eq!(
        w.bell(),
        pending,
        "the bell is approvals plus questions (none)"
    );
    assert_eq!(w.projects()[a].status(), ProjectStatus::NeedsInput);
    assert_eq!(w.hamburger(), ProjectStatus::NeedsInput);
    assert!(w.projects()[a].state_line().starts_with("Needs input"));

    // Answer every ask as it comes; the count falls to zero when none waits.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "the run never ended");
        let host = w.projects_mut()[a]
            .interaction
            .as_mut()
            .expect("interaction")
            .host_mut();
        for ask in host.pending_asks() {
            let _ = BoxBackend::answer(host, ask.id, kgraf_orchestrator::AskAnswer::AllowOnce);
        }
        let done = host.running().is_none();
        w.tick(&ctx);
        if done && w.bell() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(w.projects()[a].status(), ProjectStatus::NeedsInput);
}

/// The switcher's list survives a restart: the saved folders reopen in order,
/// the saved active one is shown, and a `--project` folder is added and shown.
/// The list goes through the settings file (configuration, 0017).
#[test]
fn open_projects_are_restored_in_order_with_the_active_one_shown() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().expect("dir")).collect();
    let roots: Vec<std::path::PathBuf> = dirs
        .iter()
        .map(|d| kgraf_shell::projects::canonical(d.path()))
        .collect();
    let cfg = tempfile::tempdir().expect("cfg");
    let path = cfg.path().join("settings.toml");
    let saved = kgraf_shell::settings::Settings {
        projects: roots[..2].to_vec(),
        active_project: Some(roots[1].clone()),
        ..Default::default()
    };
    saved.save(&path).expect("save");
    let loaded = kgraf_shell::settings::Settings::load(&path).expect("load");

    let mut opened = Vec::new();
    let w = Workspace::restore(
        &loaded.projects,
        loaded.active_project.as_deref(),
        None,
        |r| {
            opened.push(r.to_path_buf());
            ProjectParts::default()
        },
    );
    assert_eq!(opened, roots[..2]);
    assert_eq!(w.roots(), roots[..2]);
    assert_eq!(w.active().map(|p| p.root.clone()), Some(roots[1].clone()));

    // With `--project`: added at the end and shown; a folder already open is not
    // opened twice.
    let w = Workspace::restore(
        &loaded.projects,
        loaded.active_project.as_deref(),
        Some(&roots[2]),
        |_| ProjectParts::default(),
    );
    assert_eq!(w.roots(), roots);
    assert_eq!(w.active().map(|p| p.root.clone()), Some(roots[2].clone()));
    let w = Workspace::restore(&loaded.projects, None, Some(&roots[0]), |_| {
        ProjectParts::default()
    });
    assert_eq!(w.roots(), roots[..2], "no duplicate");
    assert_eq!(w.active_index(), Some(0));
}

/// A project that works cannot be closed. A run hosted in its box
/// holds it open, and so does work its admission has in flight without a box
/// run (a scheduled run is admitted through the same host). When the work ends,
/// it closes.
#[test]
fn a_working_project_is_not_closed() {
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let (a_root, hp, factory) = hosted_project(plan, Duration::from_millis(400), false);
    let ctx = egui::Context::default();
    let mut w = Workspace::default();
    let a = w.open(a_root.path().to_path_buf(), || ProjectParts {
        host: Some(hp),
        ..ProjectParts::default()
    });
    w.tick(&ctx);
    // A scheduled run's dispatch: admitted on the project's admission, no box run.
    let admitted = factory.parts.admission.gate().try_admit(1).expect("admit");
    let err = w
        .close(a)
        .expect_err("work in flight holds the project open");
    assert!(err.contains("working"), "{err}");
    factory.parts.admission.gate().release(admitted);

    // A box run.
    w.projects_mut()[a]
        .interaction
        .as_mut()
        .expect("interaction")
        .host_mut()
        .start("make a")
        .expect("start");
    let err = w.close(a).expect_err("a hosted run holds the project open");
    assert!(err.contains("working"), "{err}");
    tick_until(&ctx, &mut w, "the run ends", |w| {
        w.projects()[a]
            .interaction
            .as_ref()
            .is_some_and(|i| i.host().running().is_none())
            && !w.projects()[a].signals().working
    });
    w.close(a).expect("closed once idle");
    assert!(w.projects().is_empty());
}

/// The shown project's results are seen as they land, so its own
/// finished run does not light it cyan.
#[test]
fn results_landing_in_the_shown_project_are_seen() {
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let (a_root, hp, _factory) = hosted_project(plan, Duration::from_millis(20), false);
    let ctx = egui::Context::default();
    let mut w = Workspace::default();
    let a = w.open(a_root.path().to_path_buf(), || ProjectParts {
        host: Some(hp),
        ..ProjectParts::default()
    });
    w.tick(&ctx);
    w.projects_mut()[a]
        .interaction
        .as_mut()
        .expect("interaction")
        .host_mut()
        .start("make a")
        .expect("start");
    tick_until(&ctx, &mut w, "the result is read", |w| {
        w.projects()[a].signals().results > 0 && !w.projects()[a].signals().working
    });
    assert_eq!(
        w.projects()[a].status(),
        ProjectStatus::Idle,
        "shown: nothing is new"
    );
}

/// A factory whose every run fails to build: a box run that fails.
struct Failing;

#[async_trait::async_trait]
impl OrchestratorFactory for Failing {
    async fn configured(&self) -> Result<(), ReadinessFailure> {
        Ok(())
    }
    fn build(&self, _run: &RunId) -> Result<Orchestrator, OrchestratorError> {
        Err(OrchestratorError::NoRoutes)
    }
}

fn failing_parts() -> HostParts {
    let admission = kgraf_orchestrator::Admission::new(kgraf_orchestrator::Limits::default());
    HostParts {
        factory: Arc::new(Failing),
        housekeeping: Arc::new(FixedAdmission(admission)),
        desk: Arc::new(AskDesk::new(None, Arc::new(|| {}))),
        router: None,
    }
}

/// A failed box run is a pinned failure card that needs input
/// (yellow, counted by the bell) until it is dismissed; the dismissal is kept
/// on disk; a new run's failure shows again.
#[test]
fn a_failed_run_needs_input_until_dismissed() {
    let root = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(root.path().join(".kgraf")).expect("state");
    let ctx = egui::Context::default();
    let mut w = Workspace::default();
    let a = w.open(root.path().to_path_buf(), || ProjectParts {
        host: Some(failing_parts()),
        ..ProjectParts::default()
    });
    w.tick(&ctx);
    assert_eq!(w.bell(), 0);
    let start = |w: &mut Workspace| {
        w.projects_mut()[a]
            .interaction
            .as_mut()
            .expect("interaction")
            .host_mut()
            .start("fail")
            .expect("start")
    };
    start(&mut w);
    tick_until(&ctx, &mut w, "the run fails", |w| {
        !w.projects()[a].failures().is_empty()
    });
    let failures = w.projects()[a].failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    let f = failures[0].clone();
    assert!(f.dismissable && f.key.starts_with("run:"), "{f:?}");
    assert_eq!(w.projects()[a].status(), ProjectStatus::NeedsInput);
    assert_eq!(w.bell(), 1, "the bell counts the failure");

    w.projects_mut()[a].dismiss(&f.key).expect("dismiss");
    w.tick(&ctx);
    assert!(w.projects()[a].failures().is_empty());
    assert_eq!(w.bell(), 0);
    assert_ne!(w.projects()[a].status(), ProjectStatus::NeedsInput);
    // Kept on disk: a restart does not bring it back.
    assert!(kgraf_shell::projects::load_acked(&w.projects()[a].root).contains(&f.key));

    // Final check item 1: resuming the same run id is a new attempt, and its
    // failure is a new card even though the first was dismissed.
    let run = f
        .key
        .strip_prefix("run:")
        .and_then(|k| k.rsplit_once('@'))
        .map(|(run, _)| run.to_owned())
        .expect("the key names the run");
    w.projects_mut()[a]
        .interaction
        .as_mut()
        .expect("interaction")
        .host_mut()
        .resume(kgraf_core::RunId::new(run.clone()))
        .expect("resume");
    tick_until(&ctx, &mut w, "the resumed run fails", |w| {
        !w.projects()[a].failures().is_empty()
    });
    let again = w.projects()[a].failures();
    assert_eq!(again.len(), 1, "{again:?}");
    assert_ne!(again[0].key, f.key, "a new attempt, a new key");
    assert!(again[0].words.contains(&run), "the same run: {again:?}");
    assert_eq!(w.bell(), 1);
    w.projects_mut()[a].dismiss(&again[0].key).expect("dismiss");

    // A new run: its failure is a new card.
    start(&mut w);
    tick_until(&ctx, &mut w, "the second run fails", |w| {
        w.projects()[a]
            .failures()
            .first()
            .is_some_and(|g| g.key != f.key && g.key != again[0].key)
    });
    assert_eq!(w.bell(), 1);
}
