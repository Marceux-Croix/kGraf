//! Decisions 0093 and 0094 through a real hosted run: a worker refused a
//! command by policy reports a permission shortfall through
//! `report_shortfall`, and the user's permission mode decides: Orchestrator
//! decides grants it and the fresh attempt runs the command; Manual with
//! nobody attending denies it and says how to allow it; Manual with a person
//! attending asks on the desk, and a grant or a mode change releases it;
//! Auto, Accept edits and Bypass let the command run with nothing to report;
//! Bypass still honors a deny rule. Plan review holds dispatch until the
//! user's yes, re-plans on a change, and fails loud with nobody to approve.
//! A grant that did not help holds rather than repeating; a worker that
//! finished after a refusal it never retried is reported unverified. The
//! model is a script; the host, the orchestrator, the worker loop, the
//! approval chain, the shell tool and the sandbox are real. A child of
//! `tests.rs`, for its fixtures.

use super::*;
use kgraf_provider::{Content, Role, StopReason};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Written by the brief of an attempt whose contract holds a grant
/// (`shortfall_brief`).
const GRANT_MARK: &str = "granted them to this contract";

/// What a scripted worker does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Worker {
    /// Runs `cp`; if refused, reports the shortfall; with a grant, runs `cp`
    /// and answers.
    ReportsThenRuns,
    /// Reports a `cp` shortfall on its first round, grant or not.
    AlwaysReports,
    /// Runs `cp`, is refused, then writes the file with apply_patch anyway
    /// and answers: the worker that guesses.
    RefusedThenWrites,
    /// Writes the file with apply_patch and answers.
    Writes,
}

struct Script {
    worker: Worker,
    /// Worker attempts started (round-0 worker requests).
    attempts: AtomicUsize,
    /// A worker request carried the grant in its brief.
    saw_grant: AtomicBool,
    /// A granted attempt's `cp` ran and exited 0.
    granted_ran: AtomicBool,
    /// Decomposition requests answered.
    planner_calls: AtomicUsize,
    /// The newest decomposition request, as text.
    last_planner_request: std::sync::Mutex<String>,
}

impl Script {
    fn new(worker: Worker) -> Arc<Self> {
        Arc::new(Self {
            worker,
            attempts: AtomicUsize::new(0),
            saw_grant: AtomicBool::new(false),
            granted_ran: AtomicBool::new(false),
            planner_calls: AtomicUsize::new(0),
            last_planner_request: std::sync::Mutex::new(String::new()),
        })
    }
}

fn tool(name: &str, input: serde_json::Value, round: usize) -> Content {
    Content::ToolUse {
        id: format!("call-{round}"),
        name: name.into(),
        input,
        signature: None,
    }
}

fn cp() -> serde_json::Value {
    serde_json::json!({"command": ["cp", "README.md", "a.txt"]})
}

fn report() -> serde_json::Value {
    serde_json::json!({
        "kind": "permission",
        "need": "cp",
        "why": "the instruction asks for a.txt to be copied from README.md with cp"
    })
}

fn patch() -> serde_json::Value {
    serde_json::json!({
        "patch": "*** Begin Patch\n*** Add File: a.txt\n+copied\n*** End Patch"
    })
}

#[async_trait::async_trait]
impl Provider for Script {
    fn adapter(&self) -> &'static str {
        "script"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        request: &kgraf_provider::Request,
        token: &kgraf_provider::CallToken,
    ) -> std::result::Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
        let system: String = request.system.iter().filter_map(Content::as_text).collect();
        let (content, stop_reason) = if system.contains("Split the user's instruction") {
            self.planner_calls.fetch_add(1, Ordering::SeqCst);
            *self
                .last_planner_request
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = format!("{system}{:?}", request.messages);
            (
                vec![Content::text(
                    r#"{"contracts":[{"id":"a","instruction":"copy README.md to a.txt","acceptance":"a.txt exists","writes":[{"path":"a.txt"}],"tools":[]}]}"#,
                )],
                StopReason::EndTurn,
            )
        } else if system.contains("still serves") {
            (
                vec![Content::text(r#"{"serves":true,"reason":"on track"}"#)],
                StopReason::EndTurn,
            )
        } else {
            let round = request
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count();
            let seen = format!("{:?}", request.messages);
            let granted = seen.contains(GRANT_MARK);
            if round == 0 {
                self.attempts.fetch_add(1, Ordering::SeqCst);
            }
            if granted {
                self.saw_grant.store(true, Ordering::SeqCst);
                if round == 1 && seen.contains("exit status: 0") {
                    self.granted_ran.store(true, Ordering::SeqCst);
                }
            }
            let call = match (self.worker, granted, round) {
                (Worker::ReportsThenRuns, _, 0) => Some(tool("shell", cp(), round)),
                // A `cp` that ran needs no report.
                (Worker::ReportsThenRuns, false, 1) if !seen.contains("exit status: 0") => {
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, report(), round))
                }
                (Worker::AlwaysReports, _, 0) => {
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, report(), round))
                }
                (Worker::RefusedThenWrites, _, 0) => Some(tool("shell", cp(), round)),
                (Worker::RefusedThenWrites, _, 1) => Some(tool("apply_patch", patch(), round)),
                (Worker::Writes, _, 0) => Some(tool("apply_patch", patch(), round)),
                _ => None,
            };
            match call {
                Some(c) => (vec![c], StopReason::ToolUse),
                None => (vec![Content::text("done")], StopReason::EndTurn),
            }
        };
        let usage = kgraf_core::TokenUsage {
            uncached_input: 10,
            text_output: 10,
            ..kgraf_core::TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(kgraf_provider::Response {
            model: request.model.clone(),
            content,
            stop_reason,
            usage,
            request_id: None,
        })
    }
}

fn control(mode: kgraf_tools::PermissionMode) -> Arc<kgraf_tools::PermissionControl> {
    control_with(mode, kgraf_tools::permissions::AllowSet::default(), false)
}

fn control_with(
    mode: kgraf_tools::PermissionMode,
    allow: kgraf_tools::permissions::AllowSet,
    plan_review: bool,
) -> Arc<kgraf_tools::PermissionControl> {
    Arc::new(
        kgraf_tools::PermissionControl::new(
            kgraf_tools::PermissionSettings {
                mode,
                allow,
                plan_review,
                ..kgraf_tools::PermissionSettings::default()
            },
            kgraf_tools::ModeSource::CommandLine,
            kgraf_tools::permissions::ProcessFacts::default(),
        )
        .expect("control"),
    )
}

/// One hosted run of `worker` to its end, nobody attending, under the
/// permission control `permissions`, with `verify` as the acceptance command.
/// Returns the report and the trace text.
async fn run(
    worker: Worker,
    permissions: Arc<kgraf_tools::PermissionControl>,
    verify: &str,
    id: &str,
) -> (kgraf_orchestrator::RunReport, String, Arc<Script>) {
    let (_dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = verify.into();
    let root = config.root.clone();
    let script = Script::new(worker);
    let host = Host::open(config)
        .expect("open")
        .with_permissions(permissions)
        .with_route_provider("mine", script.clone())
        .expect("a used route");
    let outcome = host
        .prepare(RunId::new(id), None, &mut |_| {})
        .await
        .expect("prepare")
        .run(RunStart::Instruction("copy README.md to a.txt".into()))
        .await
        .expect("run");
    let trace = std::fs::read_to_string(root.join(".kgraf/runs").join(id).join("trace.jsonl"))
        .expect("trace");
    (outcome.report, trace, script)
}

/// A run started as the box starts one (a person attends), under
/// `permissions`. The caller answers on the returned desk and awaits the
/// handle; the host and the project directory are returned so they live as
/// long as the run.
struct Attended {
    run: tokio::task::JoinHandle<
        std::result::Result<kgraf_orchestrator::RunReport, kgraf_orchestrator::OrchestratorError>,
    >,
    desk: Arc<kgraf_orchestrator::ReviewDesk>,
    script: Arc<Script>,
    root: PathBuf,
    _host: Host,
    _dir: tempfile::TempDir,
}

async fn attended(
    worker: Worker,
    permissions: Arc<kgraf_tools::PermissionControl>,
    id: &str,
) -> Attended {
    let (dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = "test -f a.txt".into();
    let root = config.root.clone();
    let script = Script::new(worker);
    let host = Host::open(config)
        .expect("open")
        .with_permissions(permissions)
        .with_route_provider("mine", script.clone())
        .expect("a used route");
    let orchestrator = host
        .prepare(RunId::new(id), None, &mut |_| {})
        .await
        .expect("prepare")
        .into_orchestrator(&host)
        .with_question_attendance(kgraf_orchestrator::QuestionAttendance::Box);
    let desk = orchestrator.review_desk();
    let id = RunId::new(id);
    let run = tokio::spawn(async move {
        orchestrator
            .start(id, "copy README.md to a.txt".into())
            .await
    });
    Attended {
        run,
        desk,
        script,
        root,
        _host: host,
        _dir: dir,
    }
}

async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..2_000 {
        if let Some(v) = probe() {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn shortfall_lines(trace: &str) -> Vec<&str> {
    trace
        .lines()
        .filter(|l| l.contains(r#""event":"shortfall""#))
        .collect()
}

fn permission_lines(trace: &str) -> Vec<&str> {
    trace
        .lines()
        .filter(|l| l.contains(r#""event":"permission""#))
        .collect()
}

/// Orchestrator decides: the worker's refused `cp` is reported, the
/// orchestrator grants it to the contract (recorded in the trace and the
/// graph), and the fresh attempt runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orchestrator_decides_grants_a_shortfall_and_the_next_attempt_runs_it() {
    let (report, trace, script) = run(
        Worker::ReportsThenRuns,
        control(kgraf_tools::PermissionMode::OrchestratorDecides),
        "test -f a.txt",
        "sf-grant",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    assert_eq!(report.verified, vec!["a".to_owned()]);
    assert!(report.unverified.is_empty(), "{:?}", report.unverified);
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(
        lines[0].contains("granted `cp` (orchestrator)"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains(r#""kind":"permission""#), "{}", lines[0]);
    assert!(
        permission_lines(&trace)
            .iter()
            .any(|l| l.contains(r#""what":"granted""#) && l.contains("`cp`")),
        "{trace}"
    );
    assert!(script.saw_grant.load(Ordering::SeqCst));
    assert!(
        script.granted_ran.load(Ordering::SeqCst),
        "the fresh attempt ran the granted command"
    );
    assert_eq!(script.attempts.load(Ordering::SeqCst), 2);
}

/// Manual, nobody attending: what would ask is denied, the hold says how to
/// allow it, and nothing is granted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_headless_denies_a_shortfall_and_names_how_to_allow_it() {
    let (report, trace, script) = run(
        Worker::ReportsThenRuns,
        control(kgraf_tools::PermissionMode::Manual),
        "test -f a.txt",
        "sf-hold",
    )
    .await;
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}\n{trace}");
    };
    assert!(reason.contains("denied:"), "{reason}");
    assert!(reason.contains("--allow cp"), "{reason}");
    assert!(report.verified.is_empty());
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(lines[0].contains("held:"), "{}", lines[0]);
    assert!(!script.saw_grant.load(Ordering::SeqCst), "nothing granted");
    assert_eq!(
        script.attempts.load(Ordering::SeqCst),
        1,
        "no second attempt"
    );
}

/// Auto with `cp` in the allow set, and Bypass: the command runs on the
/// first attempt, with no refusal to report, and the mode's grant is
/// recorded. Accept edits does the same for `cp`, a file program.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_accept_edits_and_bypass_run_the_command_without_a_shortfall() {
    let auto = control_with(
        kgraf_tools::PermissionMode::Auto,
        kgraf_tools::permissions::AllowSet::default()
            .with_programs(["cp"])
            .expect("allow"),
        false,
    );
    for (name, permissions) in [
        ("sf-auto", auto),
        (
            "sf-edits",
            control(kgraf_tools::PermissionMode::AcceptEdits),
        ),
        ("sf-bypass", control(kgraf_tools::PermissionMode::Bypass)),
    ] {
        let (report, trace, script) =
            run(Worker::ReportsThenRuns, permissions, "test -f a.txt", name).await;
        assert_eq!(
            report.status,
            kgraf_orchestrator::RunStatus::Completed,
            "{name}: {report:?}\n{trace}"
        );
        assert!(shortfall_lines(&trace).is_empty(), "{name}: {trace}");
        assert_eq!(script.attempts.load(Ordering::SeqCst), 1, "{name}");
        assert!(
            permission_lines(&trace)
                .iter()
                .any(|l| l.contains(r#""what":"granted""#) && l.contains("`cp`")),
            "{name}: the mode's grant is recorded\n{trace}"
        );
    }
}

/// Bypass does not lift the user's deny rule: `cp` is refused by the rule,
/// the orchestrator's grant does not help (the rule still refuses), and the
/// contract holds without the command ever running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_still_honors_a_deny_rule_through_a_hosted_run() {
    use kgraf_tools::permissions::{Rule, UserRules};
    let permissions = Arc::new(
        kgraf_tools::PermissionControl::new(
            kgraf_tools::PermissionSettings {
                mode: kgraf_tools::PermissionMode::Bypass,
                rules: UserRules {
                    deny: vec![Rule::parse("shell(cp*)").expect("rule")],
                    ask: vec![],
                },
                ..kgraf_tools::PermissionSettings::default()
            },
            kgraf_tools::ModeSource::CommandLine,
            kgraf_tools::permissions::ProcessFacts::default(),
        )
        .expect("bypass"),
    );
    let (report, trace, script) = run(
        Worker::ReportsThenRuns,
        permissions,
        "test -f a.txt",
        "sf-deny",
    )
    .await;
    assert!(
        matches!(report.status, kgraf_orchestrator::RunStatus::Blocked { .. }),
        "{report:?}\n{trace}"
    );
    assert!(report.verified.is_empty());
    assert!(
        !script.granted_ran.load(Ordering::SeqCst),
        "the denied command never ran"
    );
}

/// A person attends, Manual: the shortfall waits on the desk as a request;
/// the user's grant releases the contract and the fresh attempt runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_attended_asks_the_user_and_a_grant_releases_the_contract() {
    let a = attended(
        Worker::ReportsThenRuns,
        control(kgraf_tools::PermissionMode::Manual),
        "sf-ask",
    )
    .await;
    let request = until("a permission request", || {
        a.desk.pending_permissions().into_iter().next()
    })
    .await;
    assert_eq!(request.subject, "cp");
    assert_eq!(a.script.attempts.load(Ordering::SeqCst), 1);
    a.desk.answer_permission(request.id, true).expect("grant");
    let report = a.run.await.expect("join").expect("run");
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}"
    );
    assert!(a.script.granted_ran.load(Ordering::SeqCst));
    let trace =
        std::fs::read_to_string(a.root.join(".kgraf/runs/sf-ask/trace.jsonl")).expect("trace");
    assert!(
        permission_lines(&trace)
            .iter()
            .any(|l| l.contains(r#""what":"granted""#) && l.contains("(user)")),
        "{trace}"
    );
}

/// A mode change reaches a request already waiting: switching from Manual to
/// Orchestrator decides grants it at once, without the user's answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mode_change_releases_a_waiting_request() {
    let permissions = control(kgraf_tools::PermissionMode::Manual);
    let a = attended(Worker::ReportsThenRuns, permissions.clone(), "sf-switch").await;
    until("a permission request", || {
        a.desk.pending_permissions().into_iter().next()
    })
    .await;
    permissions
        .set_mode(
            kgraf_tools::PermissionMode::OrchestratorDecides,
            kgraf_tools::ModeSource::UserInterface,
        )
        .expect("mode");
    let report = a.run.await.expect("join").expect("run");
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}"
    );
    assert!(a.desk.pending_permissions().is_empty());
    let trace =
        std::fs::read_to_string(a.root.join(".kgraf/runs/sf-switch/trace.jsonl")).expect("trace");
    let lines = permission_lines(&trace);
    assert!(
        lines.iter().any(|l| l.contains(r#""what":"mode_changed""#)),
        "the mode change is recorded\n{trace}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains(r#""what":"granted""#) && l.contains("(orchestrator)")),
        "{trace}"
    );
}

/// Bounded: a grant that did not help is not granted again; the second
/// report holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_that_did_not_help_holds_instead_of_repeating() {
    let (report, trace, script) = run(
        Worker::AlwaysReports,
        control(kgraf_tools::PermissionMode::OrchestratorDecides),
        "test -f a.txt",
        "sf-bound",
    )
    .await;
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}\n{trace}");
    };
    assert!(reason.contains("did not help"), "{reason}");
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 2, "{trace}");
    assert!(lines[0].contains("granted `cp`"), "{}", lines[0]);
    assert!(lines[1].contains("held:"), "{}", lines[1]);
    assert_eq!(script.attempts.load(Ordering::SeqCst), 2);
}

/// A worker that finished after a refusal it never retried, or a contract
/// whose check is `true`, is reported unverified, never plainly done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_then_finished_worker_or_a_vacuous_check_is_unverified() {
    let manual = || control(kgraf_tools::PermissionMode::Manual);
    let (report, trace, _) = run(
        Worker::RefusedThenWrites,
        manual(),
        "test -f a.txt",
        "sf-unverified",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    assert_eq!(report.unverified.len(), 1, "{report:?}");
    assert_eq!(report.unverified[0].0, "a");
    assert!(
        report.unverified[0].1.contains("never ran it") && report.unverified[0].1.contains("`cp`"),
        "{:?}",
        report.unverified
    );
    assert!(trace.contains(r#""event":"unverified""#), "{trace}");

    let (report, _, _) = run(Worker::Writes, manual(), "true", "sf-vacuous").await;
    assert_eq!(report.status, kgraf_orchestrator::RunStatus::Completed);
    assert_eq!(report.unverified.len(), 1, "{report:?}");
    assert!(
        report.unverified[0].1.contains("checks nothing"),
        "{:?}",
        report.unverified
    );

    let (report, _, _) = run(Worker::Writes, manual(), "test -f a.txt", "sf-verified").await;
    assert_eq!(report.status, kgraf_orchestrator::RunStatus::Completed);
    assert!(report.unverified.is_empty(), "{report:?}");
}

/// The run file cannot set a permission mode, Bypass, an allow set or plan
/// review: each key is refused, naming where the setting lives.
#[test]
fn the_run_file_cannot_set_the_permission_mode_or_plan_review() {
    let (dir, config) = project();
    for key in [
        r#""container_sandbox": true"#,
        r#""may_grant": ["python3"]"#,
        r#""permission_mode": "bypass""#,
        r#""mode": "bypass""#,
        r#""allow_bypass": true"#,
        r#""plan_review": false"#,
        r#""approve_plan": true"#,
        r#""allow": {"programs": ["python3"]}"#,
    ] {
        let path = dir.path().join("run.json");
        std::fs::write(
            &path,
            format!(r#"{{"routes": ["ollama:llama3"], "prices": "prices.json", {key}}}"#),
        )
        .expect("write");
        let loaded = RunConfig::load(&config.root, &path);
        let err = loaded.expect_err(key).to_string();
        assert!(err.contains("which only you set"), "{key}: {err}");
    }
}

/// Plan review on, a person attends: no worker starts until the plan is
/// approved; the plan names what each contract writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_review_blocks_dispatch_until_the_user_approves() {
    let a = attended(
        Worker::Writes,
        control_with(
            kgraf_tools::PermissionMode::Manual,
            Default::default(),
            true,
        ),
        "pr-approve",
    )
    .await;
    let plan = until("the plan", || a.desk.pending_plan()).await;
    assert_eq!(plan.revision, 1);
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.touched_paths(), vec!["a.txt".to_owned()]);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        a.script.attempts.load(Ordering::SeqCst),
        0,
        "no worker before the yes"
    );
    a.desk
        .answer_plan(kgraf_orchestrator::PlanAnswer::Approve)
        .expect("approve");
    let report = a.run.await.expect("join").expect("run");
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}"
    );
    assert_eq!(a.script.attempts.load(Ordering::SeqCst), 1);
}

/// Change sends a note to the planner and shows a new plan; Reject ends the
/// run with nothing dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_re_plans_with_the_note_and_a_rejection_dispatches_nothing() {
    let a = attended(
        Worker::Writes,
        control_with(
            kgraf_tools::PermissionMode::Manual,
            Default::default(),
            true,
        ),
        "pr-change",
    )
    .await;
    until("the plan", || a.desk.pending_plan()).await;
    a.desk
        .answer_plan(kgraf_orchestrator::PlanAnswer::Change {
            note: "name the file b-side".into(),
        })
        .expect("change");
    let second = until("the second plan", || {
        a.desk.pending_plan().filter(|p| p.revision == 2)
    })
    .await;
    assert_eq!(second.items.len(), 1);
    assert_eq!(a.script.planner_calls.load(Ordering::SeqCst), 2);
    assert!(
        a.script
            .last_planner_request
            .lock()
            .expect("lock")
            .contains("name the file b-side"),
        "the planner was told the change"
    );
    a.desk
        .answer_plan(kgraf_orchestrator::PlanAnswer::Reject {
            reason: "not now".into(),
        })
        .expect("reject");
    let report = a.run.await.expect("join").expect("run");
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}");
    };
    assert!(reason.contains("rejected the plan"), "{reason}");
    assert_eq!(a.script.attempts.load(Ordering::SeqCst), 0);
}

/// Plan review on with nobody attending fails loud before any call, unless
/// the command line approves the plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn headless_plan_review_fails_loud_unless_approved_on_the_command_line() {
    let review = || {
        control_with(
            kgraf_tools::PermissionMode::Manual,
            Default::default(),
            true,
        )
    };
    let (_dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = "test -f a.txt".into();
    let script = Script::new(Worker::Writes);
    let host = Host::open(config.clone())
        .expect("open")
        .with_permissions(review())
        .with_route_provider("mine", script.clone())
        .expect("route");
    let err = host
        .prepare(RunId::new("pr-headless"), None, &mut |_| {})
        .await
        .expect("prepare")
        .run(RunStart::Instruction("copy README.md to a.txt".into()))
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("plan review is on"), "{err}");
    assert_eq!(
        script.planner_calls.load(Ordering::SeqCst),
        0,
        "nothing paid for"
    );

    config.approve_plan = true;
    let host = Host::open(config)
        .expect("open")
        .with_permissions(review())
        .with_route_provider("mine", script.clone())
        .expect("route");
    let outcome = host
        .prepare(RunId::new("pr-auto"), None, &mut |_| {})
        .await
        .expect("prepare")
        .run(RunStart::Instruction("copy README.md to a.txt".into()))
        .await
        .expect("run");
    assert_eq!(
        outcome.report.status,
        kgraf_orchestrator::RunStatus::Completed
    );
}
