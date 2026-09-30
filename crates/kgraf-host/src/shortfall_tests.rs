//! Decision 0093 through a real hosted run: a worker refused a command by
//! policy reports a permission shortfall through `report_shortfall`; the
//! orchestrator grants within the operator's ceiling and the fresh attempt
//! runs the command, or holds with a question and grants nothing; a grant
//! that did not help holds rather than repeating; and a worker that finished
//! after a refusal it never retried is reported unverified. The model is a
//! script; the host, the orchestrator, the worker loop, the approval chain
//! (unattended), the shell tool and the sandbox are real. A child of
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
}

impl Script {
    fn new(worker: Worker) -> Arc<Self> {
        Arc::new(Self {
            worker,
            attempts: AtomicUsize::new(0),
            saw_grant: AtomicBool::new(false),
            granted_ran: AtomicBool::new(false),
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
                (Worker::ReportsThenRuns, false, 1) => {
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

/// One hosted run of `worker` to its end, with `may_grant` as the ceiling and
/// `verify` as the acceptance command. Returns the report and the trace text.
async fn run(
    worker: Worker,
    may_grant: &[&str],
    verify: &str,
    id: &str,
) -> (kgraf_orchestrator::RunReport, String, Arc<Script>) {
    let (_dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = verify.into();
    config.may_grant = may_grant.iter().map(|p| (*p).to_owned()).collect();
    let root = config.root.clone();
    let script = Script::new(worker);
    let host = Host::open(config)
        .expect("open")
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

fn shortfall_lines(trace: &str) -> Vec<&str> {
    trace
        .lines()
        .filter(|l| l.contains(r#""event":"shortfall""#))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_shortfall_within_the_ceiling_is_granted_and_the_next_attempt_runs_it() {
    let (report, trace, script) = run(
        Worker::ReportsThenRuns,
        &["cp"],
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
    assert!(lines[0].contains("granted `cp`"), "{}", lines[0]);
    assert!(lines[0].contains(r#""kind":"permission""#), "{}", lines[0]);
    assert!(script.saw_grant.load(Ordering::SeqCst));
    assert!(
        script.granted_ran.load(Ordering::SeqCst),
        "the fresh attempt ran the granted command"
    );
    assert_eq!(script.attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_shortfall_beyond_the_ceiling_holds_with_a_question_and_grants_nothing() {
    let (report, trace, script) =
        run(Worker::ReportsThenRuns, &[], "test -f a.txt", "sf-hold").await;
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}\n{trace}");
    };
    assert!(reason.contains("--may-grant cp"), "{reason}");
    assert!(report.verified.is_empty());
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(lines[0].contains("held for the user"), "{}", lines[0]);
    assert!(!script.saw_grant.load(Ordering::SeqCst), "nothing granted");
    assert_eq!(
        script.attempts.load(Ordering::SeqCst),
        1,
        "no second attempt"
    );
}

/// Bounded: a grant that did not help is not granted again; the second
/// report holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_that_did_not_help_holds_instead_of_repeating() {
    let (report, trace, script) =
        run(Worker::AlwaysReports, &["cp"], "test -f a.txt", "sf-bound").await;
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}\n{trace}");
    };
    assert!(reason.contains("did not help"), "{reason}");
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 2, "{trace}");
    assert!(lines[0].contains("granted `cp`"), "{}", lines[0]);
    assert!(lines[1].contains("held for the user"), "{}", lines[1]);
    assert_eq!(script.attempts.load(Ordering::SeqCst), 2);
}

/// A worker that finished after a refusal it never retried, or a contract
/// whose check is `true`, is reported unverified, never plainly done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_then_finished_worker_or_a_vacuous_check_is_unverified() {
    let (report, trace, _) = run(
        Worker::RefusedThenWrites,
        &[],
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

    let (report, _, _) = run(Worker::Writes, &[], "true", "sf-vacuous").await;
    assert_eq!(report.status, kgraf_orchestrator::RunStatus::Completed);
    assert_eq!(report.unverified.len(), 1, "{report:?}");
    assert!(
        report.unverified[0].1.contains("checks nothing"),
        "{:?}",
        report.unverified
    );

    let (report, _, _) = run(Worker::Writes, &[], "test -f a.txt", "sf-verified").await;
    assert_eq!(report.status, kgraf_orchestrator::RunStatus::Completed);
    assert!(report.unverified.is_empty(), "{report:?}");
}

/// The run file cannot turn on the container setting or set a ceiling:
/// neither key is part of its format, and a file naming one is refused.
#[test]
fn the_run_file_cannot_set_the_container_sandbox_or_the_grant_ceiling() {
    let (dir, config) = project();
    for key in [
        r#""container_sandbox": true"#,
        r#""may_grant": ["python3"]"#,
    ] {
        let path = dir.path().join("run.json");
        std::fs::write(
            &path,
            format!(r#"{{"routes": ["ollama:llama3"], "prices": "prices.json", {key}}}"#),
        )
        .expect("write");
        let loaded = RunConfig::load(&config.root, &path);
        assert!(loaded.is_err(), "{key}: {loaded:?}");
    }
}
