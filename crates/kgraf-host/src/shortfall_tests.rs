//! Decisions 0093 and 0094 through a real hosted run: a worker refused a
//! command by policy reports a permission shortfall through
//! `report_shortfall`, and the user's permission mode decides: Orchestrator
//! decides grants it and the fresh attempt runs the command; Manual with
//! nobody attending denies it and says how to allow it; Manual with a person
//! attending asks on the desk, and a grant or a mode change releases it;
//! Auto, Accept edits and Bypass let the command run with nothing to report;
//! Bypass still honors a deny rule. Plan review holds dispatch until the
//! user's yes, re-plans on a change, and fails loud with nobody to approve.
//! A report nothing refused is answered and the worker continues; three
//! such reports end the attempt as stuck; a file another contract owns is
//! answered, never granted, even in Bypass, unless the contract's cargo
//! acceptance cannot pass without it: that holds for a re-plan. A worker that finished after a
//! refusal it never retried is reported unverified. A shortfall that would
//! hold the contract is verified in scope first: a pass merges it, a failure
//! keeps the hold. A run check a dependent split cannot pass on its first
//! contract is deferred to the root gate, which still has to pass. The
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
pub(super) enum Worker {
    /// Runs `cp`; if refused, reports the shortfall; with a grant, runs `cp`
    /// and answers.
    ReportsThenRuns,
    /// As `ReportsThenRuns`, through a wrapper: runs `env cp ...`; if
    /// refused, reports the shortfall naming `cp`, the program that runs.
    WrappedReportsThenRuns,
    /// Runs `cp`, then reports a `cp` shortfall whether or not it ran.
    AlwaysReports,
    /// Reports a `cp` shortfall every round without running anything.
    Baseless,
    /// Contract `a` writes `b.txt` (contract `b`'s file), is refused,
    /// reports it, then writes its own `a.txt`; contract `b` writes `b.txt`.
    EditsSibling,
    /// As `EditsSibling`, but contract `a` reports an `instructions`
    /// shortfall asking for `b.txt` to be changed, then writes its own file.
    InstructsSibling,
    /// Runs `cp`, is refused, then writes the file with apply_patch anyway
    /// and answers: the worker that guesses.
    RefusedThenWrites,
    /// Writes the file with apply_patch and answers.
    Writes,
    /// Runs an `sh -c` script of `mkdir` and `cp`; if refused, reports the
    /// shortfall naming `mkdir`.
    RunsScript,
    /// Writes its own `a.txt`, then tries to write `cfg.toml`, a file no
    /// contract of the plan writes, is refused, and reports a file
    /// shortfall for it: the claim is that the contract needs that file.
    WritesThenReportsUnowned,
    /// The plan of [`DEPENDENT_PLAN`]: contract `a` writes `a.txt`, contract
    /// `b`, which depends on `a`, writes `b.txt`. Each worker writes its own
    /// file and answers.
    DependentSplit,
    /// The plan of [`MEMBER_PLAN`]: contract `a` writes the root
    /// `Cargo.toml` (members `core` and `cli`) and accepts on `cargo test -p
    /// core`; `cli/Cargo.toml` belongs to `b`, which depends on `a`. The
    /// worker of `a` first removes the checkout's untracked
    /// `cli/Cargo.toml` (see [`Script::root`]), tries to write
    /// `cli/Cargo.toml`, is refused, and reports a file shortfall for it.
    NeedsLaterMember,
    /// Runs `pip install vectorops` (a package download), is refused, and
    /// reports a permission shortfall naming `pip`; with a grant, runs it
    /// again.
    Downloads,
    /// The first attempt reports a gap in the instruction (amended); the
    /// next reports an environment fact, a missing Python module, as a paid
    /// run's worker did; answered with the facts, it writes its file.
    ModuleAfterAmendment,
    /// Every attempt reports the same gap in the instruction.
    GapTwice,
}

/// What [`Worker::ModuleAfterAmendment`] and [`Worker::GapTwice`] report.
fn gap_report() -> serde_json::Value {
    serde_json::json!({
        "kind": "instructions",
        "need": "the expected output format",
        "why": "the instruction does not say whether a.txt holds a number or a sentence"
    })
}

fn module_report() -> serde_json::Value {
    serde_json::json!({
        "kind": "instructions",
        "need": "The required Python dependency (datasets) is unavailable in this workspace.",
        "why": "Running the script failed with ModuleNotFoundError: No module named 'datasets'."
    })
}

pub(super) struct Script {
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
    /// Change requests answered (`replan_delta`, decision 0100).
    delta_calls: AtomicUsize,
    /// The newest change request, as text.
    last_delta_request: std::sync::Mutex<String>,
    /// The project checkout, for [`Worker::NeedsLaterMember`].
    root: std::sync::Mutex<Option<PathBuf>>,
    /// The failed tool results the worker requests carried, first lines
    /// each, once: what the worker's commands said when they failed.
    failed_results: std::sync::Mutex<Vec<String>>,
}

impl Script {
    pub(super) fn new(worker: Worker) -> Arc<Self> {
        Arc::new(Self {
            worker,
            attempts: AtomicUsize::new(0),
            saw_grant: AtomicBool::new(false),
            granted_ran: AtomicBool::new(false),
            planner_calls: AtomicUsize::new(0),
            last_planner_request: std::sync::Mutex::new(String::new()),
            delta_calls: AtomicUsize::new(0),
            last_delta_request: std::sync::Mutex::new(String::new()),
            root: std::sync::Mutex::new(None),
            failed_results: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// What the worker's failed tool calls said, for a test that expects the
    /// run to complete: a guard's or a hold's reason says the worker made no
    /// change, not that its commands could not start, so this names the
    /// cause when the platform is it.
    pub(super) fn failed_tool_results(&self) -> String {
        let seen = self
            .failed_results
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if seen.is_empty() {
            "failed tool calls: none".to_owned()
        } else {
            format!("failed tool calls:\n{}", seen.join("\n"))
        }
    }

    fn note_failed_results(&self, messages: &[kgraf_provider::Message]) {
        let mut seen = self
            .failed_results
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for content in messages.iter().flat_map(|m| m.content.iter()) {
            if let Content::ToolResult {
                content,
                is_error: true,
                ..
            } = content
            {
                let head: Vec<&str> = content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(4)
                    .collect();
                let head = head.join(" | ");
                if !seen.contains(&head) {
                    seen.push(head);
                }
            }
        }
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

fn env_cp() -> serde_json::Value {
    serde_json::json!({"command": ["env", "cp", "README.md", "a.txt"]})
}

fn script() -> serde_json::Value {
    serde_json::json!({"command": ["sh", "-c", "mkdir -p notes && cp README.md a.txt"]})
}

fn report() -> serde_json::Value {
    serde_json::json!({
        "kind": "permission",
        "need": "cp",
        "why": "the instruction asks for a.txt to be copied from README.md with cp"
    })
}

fn patch() -> serde_json::Value {
    add_file("a.txt")
}

fn add_file(path: &str) -> serde_json::Value {
    serde_json::json!({
        "patch": format!("*** Begin Patch\n*** Add File: {path}\n+copied\n*** End Patch")
    })
}

/// The two-contract plan of [`Worker::EditsSibling`]; the markers tell the
/// scripted worker which contract's brief it reads.
const SIBLING_PLAN: &str = r#"{"contracts":[{"id":"a","instruction":"TASK-A: write a.txt","acceptance":"a.txt exists","writes":[{"path":"a.txt"}],"tools":[]},{"id":"b","instruction":"TASK-B: write b.txt","acceptance":"b.txt exists","writes":[{"path":"b.txt"}],"tools":[]}]}"#;

/// A dependent split: `b` depends on `a`. A run check that needs both files
/// cannot pass on `a` alone.
const DEPENDENT_PLAN: &str = r#"{"contracts":[{"id":"a","instruction":"TASK-A: write a.txt","acceptance":"a.txt exists","writes":[{"path":"a.txt"}],"tools":[]},{"id":"b","instruction":"TASK-B: write b.txt","acceptance":"b.txt exists","writes":[{"path":"b.txt"}],"tools":[],"depends_on":["a"]}]}"#;

/// A new-workspace shape: the root manifest's contract accepts on
/// `-p core`, and the member `cli` comes from a contract that runs later.
const MEMBER_PLAN: &str = r#"{"contracts":[{"id":"a","instruction":"TASK-A: write `Cargo.toml` with members = [\"core\", \"cli\"] and the core crate","acceptance":"cargo test -p core","writes":[{"path":"Cargo.toml"},{"path":"core/Cargo.toml"},{"path":"core/src/lib.rs"}],"tools":[]},{"id":"b","instruction":"TASK-B: write the cli crate","acceptance":"cargo test --workspace","writes":[{"path":"cli/Cargo.toml"},{"path":"cli/src/main.rs"}],"tools":[],"depends_on":["a"]}]}"#;

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
                    if matches!(self.worker, Worker::EditsSibling | Worker::InstructsSibling) {
                        SIBLING_PLAN
                    } else if self.worker == Worker::DependentSplit {
                        DEPENDENT_PLAN
                    } else if self.worker == Worker::NeedsLaterMember {
                        MEMBER_PLAN
                    } else {
                        r#"{"contracts":[{"id":"a","instruction":"copy README.md to a.txt","acceptance":"a.txt exists","writes":[{"path":"a.txt"}],"tools":[]}]}"#
                    },
                )],
                StopReason::EndTurn,
            )
        } else if system.contains("re-plan only the change") {
            // A change to the shown plan (decision 0100): contract `a` writes
            // the file the change names instead, a supersession.
            self.delta_calls.fetch_add(1, Ordering::SeqCst);
            *self
                .last_delta_request
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = format!("{system}{:?}", request.messages);
            (
                vec![Content::text(
                    r#"{"kind":"delta","change":[{"id":"a","spec":{"id":"a","instruction":"copy README.md to b-side.txt","acceptance":"b-side.txt exists","writes":[{"path":"b-side.txt"}],"tools":[]},"why":"the user named the file"}]}"#,
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
            self.note_failed_results(&request.messages);
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
                (Worker::WrappedReportsThenRuns, _, 0) => Some(tool("shell", env_cp(), round)),
                (Worker::WrappedReportsThenRuns, false, 1) if !seen.contains("exit status: 0") => {
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, report(), round))
                }
                (Worker::AlwaysReports, _, 0) => Some(tool("shell", cp(), round)),
                (Worker::AlwaysReports, _, 1) => {
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, report(), round))
                }
                // Each report worded anew: identical calls would end the
                // attempt through the stall check before this bound.
                (Worker::Baseless, _, r) if r < 6 => {
                    let mut input = report();
                    input["why"] = format!("attempt {r}: cp is needed").into();
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, input, round))
                }
                (Worker::EditsSibling, _, r) if seen.contains("TASK-A") => match r {
                    0 => Some(tool("apply_patch", add_file("b.txt"), round)),
                    1 => Some(tool(
                        kgraf_runtime::SHORTFALL_TOOL,
                        serde_json::json!({
                            "kind": "permission",
                            "scope": "file",
                            "need": "write access to b.txt",
                            "why": "the integration needs b.txt changed too"
                        }),
                        round,
                    )),
                    2 => Some(tool("apply_patch", add_file("a.txt"), round)),
                    _ => None,
                },
                (Worker::InstructsSibling, _, r) if seen.contains("TASK-A") => match r {
                    0 => Some(tool(
                        kgraf_runtime::SHORTFALL_TOOL,
                        serde_json::json!({
                            "kind": "instructions",
                            "need": "The stub code in b.txt must be updated to use the new type",
                            "why": "my type now has a field and b.txt builds it as a value"
                        }),
                        round,
                    )),
                    1 => Some(tool("apply_patch", add_file("a.txt"), round)),
                    _ => None,
                },
                (Worker::EditsSibling | Worker::InstructsSibling, _, 0) => {
                    Some(tool("apply_patch", add_file("b.txt"), round))
                }
                (Worker::RefusedThenWrites, _, 0) => Some(tool("shell", cp(), round)),
                (Worker::RefusedThenWrites, _, 1) => Some(tool("apply_patch", patch(), round)),
                (Worker::Writes, _, 0) => Some(tool("apply_patch", patch(), round)),
                (Worker::WritesThenReportsUnowned, _, 0) => {
                    Some(tool("apply_patch", patch(), round))
                }
                (Worker::WritesThenReportsUnowned, _, 1) => {
                    Some(tool("apply_patch", add_file("cfg.toml"), round))
                }
                (Worker::WritesThenReportsUnowned, _, 2) => Some(tool(
                    kgraf_runtime::SHORTFALL_TOOL,
                    serde_json::json!({
                        "kind": "permission",
                        "scope": "file",
                        "need": "cfg.toml",
                        "why": "the instruction says to add a setting to cfg.toml"
                    }),
                    round,
                )),
                (Worker::DependentSplit, _, 0) if seen.contains("TASK-A") => {
                    Some(tool("apply_patch", add_file("a.txt"), round))
                }
                (Worker::DependentSplit, _, 0) => {
                    Some(tool("apply_patch", add_file("b.txt"), round))
                }
                (Worker::NeedsLaterMember, _, r) if seen.contains("TASK-A") => match r {
                    0 => {
                        // The plan was checked while the checkout held an
                        // untracked `cli/Cargo.toml`; it is gone now, as a
                        // repository view that no longer holds.
                        let root = self.root.lock().unwrap_or_else(|p| p.into_inner()).clone();
                        if let Some(root) = root {
                            let _ = std::fs::remove_file(root.join("cli/Cargo.toml"));
                        }
                        Some(tool("apply_patch", add_file("cli/Cargo.toml"), round))
                    }
                    1 => Some(tool(
                        kgraf_runtime::SHORTFALL_TOOL,
                        serde_json::json!({
                            "kind": "permission",
                            "scope": "file",
                            "need": "cli/Cargo.toml",
                            "why": "the root Cargo.toml lists cli, and cargo refuses to load the \
                                    workspace without cli/Cargo.toml"
                        }),
                        round,
                    )),
                    _ => None,
                },
                (Worker::Downloads, _, 0) => Some(tool(
                    "shell",
                    serde_json::json!({"command": ["pip", "install", "vectorops"]}),
                    round,
                )),
                (Worker::Downloads, _, 1) => {
                    let mut input = report();
                    input["need"] = "pip".into();
                    input["why"] = "the contract installs vectorops and checks the import".into();
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, input, round))
                }
                // The brief of an attempt after an amendment quotes the
                // earlier report in its tagged block.
                (Worker::ModuleAfterAmendment, _, 0) if !seen.contains("<worker_shortfall>") => {
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, gap_report(), round))
                }
                (Worker::ModuleAfterAmendment, _, 0) => {
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, module_report(), round))
                }
                // Writes only when answered: a held worker is told to stop.
                (Worker::ModuleAfterAmendment, _, 1)
                    if seen.contains("Nothing was amended or held") =>
                {
                    Some(tool("apply_patch", patch(), round))
                }
                (Worker::GapTwice, _, 0) => {
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, gap_report(), round))
                }
                (Worker::RunsScript, _, 0) => Some(tool("shell", script(), round)),
                (Worker::RunsScript, _, 1) if !seen.contains("exit status: 0") => {
                    let mut input = report();
                    input["need"] = "mkdir".into();
                    Some(tool(kgraf_runtime::SHORTFALL_TOOL, input, round))
                }
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
            finish_reason: None,
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
    // These runs script a plan whose contracts wait on one another to test what
    // happens between contracts (shortfalls, deferred checks, merges); a straight
    // chain would be one contract (decision 0014, amendment of 2026-10-14).
    config.collapse_chains = false;
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

/// What the attended run `id` under `root` said in its failed checks, and
/// what its worker's failed tool calls said ([`super::failed_verdicts`],
/// [`Script::failed_tool_results`]). Takes the fields, not the [`Attended`],
/// since awaiting its run moves the handle out.
fn attended_why(root: &Path, script: &Script, id: &str) -> String {
    let trace = std::fs::read_to_string(root.join(".kgraf/runs").join(id).join("trace.jsonl"))
        .unwrap_or_default();
    format!(
        "{}\n{}",
        super::failed_verdicts(&trace),
        script.failed_tool_results()
    )
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
        "{report:?}\n{trace}\n{}",
        script.failed_tool_results()
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

/// Decision 0094, amended 2026-10-06, through the real ask path: a worker
/// refused `env cp ...` reports the program that runs, `cp`. The refusal on
/// record names every program the command runs that lacks a grant, the
/// innermost first, so the orchestrator grants `cp` and `env` together and
/// the fresh attempt runs the command. Granting the program the worker named
/// alone, or the wrapper alone, would leave the command refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrapped_command_is_granted_with_every_program_it_runs() {
    let (report, trace, script) = run(
        Worker::WrappedReportsThenRuns,
        control(kgraf_tools::PermissionMode::OrchestratorDecides),
        "test -f a.txt",
        "sf-wrapped",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}\n{}",
        script.failed_tool_results()
    );
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(
        lines[0].contains("granted `cp` and `env` (orchestrator)"),
        "{}",
        lines[0]
    );
    assert!(
        script.granted_ran.load(Ordering::SeqCst),
        "the fresh attempt ran the wrapped command\n{trace}"
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
            "{name}: {report:?}\n{trace}\n{}",
            script.failed_tool_results()
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

/// Decision 0094, amended 2026-10-02: in Auto with `mkdir` and `cp`
/// allowed, an `sh -c` script of only those programs runs on the first
/// attempt with nothing asked, and the grants recorded are of `mkdir` and
/// `cp`. With `cp` alone allowed, the script is asked about, an unattended
/// run denies it, and the refusal the worker reports is grounded in
/// `mkdir`, the command the mode did not grant, not in the `sh` that ran it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_runs_a_script_of_allowed_programs_and_names_the_one_it_does_not_allow() {
    let allowing = |programs: &[&str]| {
        control_with(
            kgraf_tools::PermissionMode::Auto,
            kgraf_tools::permissions::AllowSet::default()
                .with_programs(programs.iter().copied())
                .expect("allow"),
            false,
        )
    };
    let (report, trace, script) = run(
        Worker::RunsScript,
        allowing(&["mkdir", "cp"]),
        "test -f a.txt",
        "sf-script",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}\n{}",
        script.failed_tool_results()
    );
    assert!(shortfall_lines(&trace).is_empty(), "{trace}");
    assert_eq!(script.attempts.load(Ordering::SeqCst), 1);
    let granted: Vec<&str> = permission_lines(&trace)
        .into_iter()
        .filter(|l| l.contains(r#""what":"granted""#))
        .collect();
    for program in ["`mkdir`", "`cp`"] {
        assert!(
            granted.iter().any(|l| l.contains(program)),
            "{program} granted by the mode\n{trace}"
        );
    }
    assert!(!granted.iter().any(|l| l.contains("`sh`")), "{trace}");

    let (report, trace, _) = run(
        Worker::RunsScript,
        allowing(&["cp"]),
        "test -f a.txt",
        "sf-script-ask",
    )
    .await;
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}\n{trace}");
    };
    assert!(reason.contains("`mkdir`"), "{reason}");
    assert!(reason.contains("--allow mkdir"), "{reason}");
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(lines[0].contains("held:"), "{}", lines[0]);
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
        "{report:?}\n{}",
        attended_why(&a.root, &a.script, "sf-ask")
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
        "{report:?}\n{}",
        attended_why(&a.root, &a.script, "sf-switch")
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

/// A report after the granted command ran is grounded in nothing: it is
/// answered, not granted again or held, and the worker goes on to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_nothing_refused_is_answered_and_the_worker_finishes() {
    let (report, trace, script) = run(
        Worker::AlwaysReports,
        control(kgraf_tools::PermissionMode::OrchestratorDecides),
        "test -f a.txt",
        "sf-bound",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}\n{}",
        script.failed_tool_results()
    );
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 2, "{trace}");
    assert!(lines[0].contains("granted `cp`"), "{}", lines[0]);
    assert!(lines[1].contains("answered:"), "{}", lines[1]);
    assert!(lines[1].contains("no policy refused `cp`"), "{}", lines[1]);
    assert_eq!(script.attempts.load(Ordering::SeqCst), 2);
}

/// A worker that keeps reporting what nothing refused cannot loop: two
/// reports are answered, the third stops it, and the attempt is stuck,
/// counted a failure. Nothing is granted or held in any attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn baseless_reports_are_answered_twice_then_the_attempt_is_stuck() {
    let (report, trace, script) = run(
        Worker::Baseless,
        control(kgraf_tools::PermissionMode::Bypass),
        "test -f a.txt",
        "sf-baseless",
    )
    .await;
    // Each stuck attempt counts toward the contract's failures, so the
    // retries run out and the run blocks; were it not a failure, the
    // contract would be re-dispatched without end.
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}\n{trace}");
    };
    let lines = shortfall_lines(&trace);
    let attempts = script.attempts.load(Ordering::SeqCst);
    assert!(
        attempts > 1,
        "retried after a stuck attempt: {reason}\n{trace}"
    );
    assert_eq!(lines.len(), 3 * attempts, "{trace}");
    for (i, line) in lines.iter().enumerate() {
        let want = if i % 3 == 2 { "stopped:" } else { "answered:" };
        assert!(line.contains(want), "line {i}: {line}");
    }
    assert!(!trace.contains("granted `cp`"), "{trace}");
    assert!(
        trace.contains(r#""kind":"stuck""#) || trace.contains("ends as stuck"),
        "{trace}"
    );
}

/// Decision 0087, amended 2026-10-09: a package download is refused in
/// every mode, Bypass included, and a worker that reports it short is not
/// granted `pip` (the next attempt would be refused again): the contract is
/// held at once with the refusal, which says why and what allows the work.
/// Before, Bypass ran the download as a confined command, and a grant in
/// another mode re-dispatched an attempt that ran it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_package_download_is_refused_and_held_with_what_allows_it_never_granted() {
    for (mode, id) in [
        (kgraf_tools::PermissionMode::Bypass, "sf-pip-bypass"),
        (
            kgraf_tools::PermissionMode::OrchestratorDecides,
            "sf-pip-od",
        ),
    ] {
        let (report, trace, script) = run(Worker::Downloads, control(mode), "true", id).await;
        assert!(
            matches!(report.status, kgraf_orchestrator::RunStatus::Blocked { .. }),
            "{mode}: {report:?}\n{trace}"
        );
        let lines = shortfall_lines(&trace);
        assert_eq!(lines.len(), 1, "{mode}: {trace}");
        assert!(
            lines[0].contains("needs `pip` to download packages, which no grant allows"),
            "{mode}: {}",
            lines[0]
        );
        assert!(lines[0].contains("decision 0087"), "{}", lines[0]);
        assert!(lines[0].contains("kgraf deps allow python"), "{}", lines[0]);
        assert!(
            !script.saw_grant.load(Ordering::SeqCst),
            "{mode}: never granted"
        );
        assert_eq!(
            script.attempts.load(Ordering::SeqCst),
            1,
            "{mode}: no attempt re-dispatched for a grant that cannot help"
        );
    }
}

/// Disjoint writes hold in Bypass: contract `a`'s worker is refused
/// `b.txt`, reports it, and is told the file belongs to contract `b`; it is
/// not granted, and the worker goes on to write its own file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_answers_a_sibling_contracts_file_and_never_grants_it() {
    let (report, trace, _) = run(
        Worker::EditsSibling,
        control(kgraf_tools::PermissionMode::Bypass),
        "test -f a.txt -o -f b.txt",
        "sf-sibling",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(lines[0].contains("belongs to contract `b`"), "{}", lines[0]);
    assert!(lines[0].contains("answered:"), "{}", lines[0]);
    // The answer names the check the verifier runs, never the plan's
    // Acceptance line, which the verifier does not run.
    assert!(
        lines[0].contains("judged by `test -f a.txt -o -f b.txt`"),
        "{}",
        lines[0]
    );
    assert!(!lines[0].contains("a.txt exists"), "{}", lines[0]);
    assert!(
        !trace.contains("writes to `b.txt`"),
        "never granted\n{trace}"
    );
    assert!(
        report.verified.contains(&"a".to_owned()) && report.verified.contains(&"b".to_owned()),
        "{report:?}"
    );
}

/// An `instructions` report that asks for a change to a file another contract
/// owns is answered, not written into the reporting contract's own
/// instruction, where it would change nothing: nothing is amended, the
/// contract is not re-dispatched for it, and the worker keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instructions_report_about_a_sibling_contracts_file_is_answered_not_amended() {
    let (report, trace, script) = run(
        Worker::InstructsSibling,
        control(kgraf_tools::PermissionMode::Manual),
        "test -f a.txt -o -f b.txt",
        "sf-instructs-sibling",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(lines[0].contains("answered:"), "{}", lines[0]);
    assert!(
        lines[0].contains("`b.txt` belongs to contract `b`")
            && lines[0].contains("Keep the public shape"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("judged by `test -f a.txt -o -f b.txt`")
            && !lines[0].contains("a.txt exists"),
        "{}",
        lines[0]
    );
    assert!(
        !trace.contains("the contract was amended"),
        "no amendment of the reporting contract\n{trace}"
    );
    assert_eq!(
        script.attempts.load(Ordering::SeqCst),
        2,
        "one attempt per contract: the report did not re-dispatch `a`"
    );
}

/// An unattended Bypass run whose worker, after one amendment, reports a
/// missing Python module as an `instructions` shortfall: the report states
/// an environment fact, so it is answered with the facts (the mode, how to
/// install) and the worker keeps working. Holding it stopped a paid run
/// whose worker never tried `pip`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_fact_after_an_amendment_is_answered_not_held() {
    let (report, trace, script) = run(
        Worker::ModuleAfterAmendment,
        control(kgraf_tools::PermissionMode::Bypass),
        "test -f a.txt",
        "sf-env-fact",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}\n{}",
        script.failed_tool_results()
    );
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 2, "{trace}");
    assert!(
        lines[0].contains("the contract was amended"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("answered: Nothing was amended or held"),
        "{}",
        lines[1]
    );
    assert!(
        lines[1].contains("a missing module, package or program"),
        "the trace names the classification: {}",
        lines[1]
    );
    assert!(!trace.contains("reported insufficient again"), "{trace}");
    assert_eq!(script.attempts.load(Ordering::SeqCst), 2);
}

/// A genuine gap in the instruction, reported again after its amendment,
/// still holds the contract in an unattended Bypass run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_genuine_instruction_gap_reported_twice_still_holds() {
    let (report, trace, _script) = run(
        Worker::GapTwice,
        control(kgraf_tools::PermissionMode::Bypass),
        "test -f a.txt",
        "sf-gap-twice",
    )
    .await;
    assert_ne!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 2, "{trace}");
    assert!(
        lines[0].contains("the contract was amended"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("held: the instructions were reported insufficient again"),
        "{}",
        lines[1]
    );
    assert!(!trace.contains("Nothing was amended or held"), "{trace}");
}

/// A worker refused `cp` that wrote the file another way, and whose real
/// check then passed, is verified: the refusal is kept as a notice and a
/// trace `notice` event, never dropped and never an unverified mark
/// (decision 0093, amended 2026-10-05). The shape seen in a real run: a
/// worker refused `grep` of its own edit ran `cargo test`, which passed,
/// and the contract was reported unverified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_beside_a_passing_check_is_a_fact_not_an_unverified_mark() {
    let (report, trace, _) = run(
        Worker::RefusedThenWrites,
        control(kgraf_tools::PermissionMode::Manual),
        "test -f a.txt",
        "sf-refused-verified",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    assert_eq!(report.verified, vec!["a".to_owned()], "{report:?}");
    assert!(report.unverified.is_empty(), "{report:?}");
    assert!(!trace.contains(r#""event":"unverified""#), "{trace}");
    let fact = "contract a: verified by `test -f a.txt`; the worker finished after a policy \
                refused `cp` and never ran it";
    assert!(
        report.notices.iter().any(|n| n == fact),
        "{:?}",
        report.notices
    );
    assert!(
        trace
            .lines()
            .any(|l| l.contains(r#""event":"notice""#) && l.contains(fact)),
        "{trace}"
    );
}

/// A contract whose check is `true` is reported unverified, never plainly
/// done, and a refusal its worker never retried is part of the reason, not
/// a notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_beside_a_vacuous_check_is_unverified() {
    let (report, trace, _) = run(
        Worker::RefusedThenWrites,
        control(kgraf_tools::PermissionMode::Manual),
        "true",
        "sf-refused-vacuous",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    assert_eq!(report.unverified.len(), 1, "{report:?}");
    assert_eq!(report.unverified[0].0, "a");
    assert_eq!(
        report.unverified[0].1,
        "the acceptance command `true` checks nothing; the worker finished after a policy \
         refused `cp` and never ran it"
    );
    assert!(trace.contains(r#""event":"unverified""#), "{trace}");
    assert!(
        !report.notices.iter().any(|n| n.contains("verified by")),
        "{:?}",
        report.notices
    );
}

/// A refused worker whose check fails merges nothing, so nothing is
/// reported verified beside the refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_beside_a_failing_check_is_never_called_verified() {
    let (report, trace, _) = run(
        Worker::RefusedThenWrites,
        control(kgraf_tools::PermissionMode::Manual),
        "test -f never.txt",
        "sf-refused-fails",
    )
    .await;
    assert!(report.verified.is_empty(), "{report:?}\n{trace}");
    assert!(
        !report.notices.iter().any(|n| n.contains("verified by")),
        "{:?}",
        report.notices
    );
}

/// A contract whose check is `true` is unverified with no refusal at all; a
/// real check with none is plainly verified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vacuous_check_is_unverified_and_a_real_one_is_not() {
    let manual = || control(kgraf_tools::PermissionMode::Manual);
    let (report, trace, script) = run(Worker::Writes, manual(), "true", "sf-vacuous").await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{}\n{}",
        super::failed_verdicts(&trace),
        script.failed_tool_results()
    );
    assert_eq!(report.unverified.len(), 1, "{report:?}");
    assert!(
        report.unverified[0].1.contains("checks nothing"),
        "{:?}",
        report.unverified
    );

    let (report, trace, script) =
        run(Worker::Writes, manual(), "test -f a.txt", "sf-verified").await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{}\n{}",
        super::failed_verdicts(&trace),
        script.failed_tool_results()
    );
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
        r#""script_constructs": ["variables"]"#,
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
        "{report:?}\n{}",
        attended_why(&a.root, &a.script, "pr-approve")
    );
    assert_eq!(a.script.attempts.load(Ordering::SeqCst), 1);
}

/// Change is one change to the shown plan (decision 0100): one delta call
/// carrying the user's words, no second decomposition, and the changed plan
/// shown; Reject ends the run with nothing dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_is_one_delta_on_the_shown_plan_and_a_rejection_dispatches_nothing() {
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
    assert_eq!(
        second.items[0].contract, "d1.a",
        "the superseded contract's new revision"
    );
    assert_eq!(second.touched_paths(), vec!["b-side.txt".to_owned()]);
    assert_eq!(
        a.script.planner_calls.load(Ordering::SeqCst),
        1,
        "no second decomposition"
    );
    assert_eq!(a.script.delta_calls.load(Ordering::SeqCst), 1);
    assert!(
        a.script
            .last_delta_request
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
    let root = config.root.clone();
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
    let trace =
        std::fs::read_to_string(root.join(".kgraf/runs/pr-auto/trace.jsonl")).unwrap_or_default();
    assert_eq!(
        outcome.report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{:?}\n{}\n{}",
        outcome.report,
        super::failed_verdicts(&trace),
        script.failed_tool_results()
    );
}

/// Decision 0093, amended 2026-10-02: a worker that wrote its own file and
/// then reported that it needs `cfg.toml`, a file no contract writes, would
/// be held as a plan-shape error. Its in-scope result is verified first; it
/// passes, so the contract merges, the shortfall is kept as a fact in a
/// notice, and `cfg.toml` is never granted or written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_shortfall_whose_in_scope_result_verifies_is_merged_and_kept_as_a_fact() {
    let (report, trace, script) = run(
        Worker::WritesThenReportsUnowned,
        control(kgraf_tools::PermissionMode::Manual),
        "test -f a.txt",
        "sf-held-verifies",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    assert_eq!(report.verified, vec!["a".to_owned()], "{report:?}");
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(lines[0].contains("plan-shape error"), "{}", lines[0]);
    let fact = report
        .notices
        .iter()
        .find(|n| n.contains("verified in its own scope"))
        .unwrap_or_else(|| panic!("the shortfall is kept as a fact: {report:?}"));
    assert!(fact.contains("`test -f a.txt`"), "{fact}");
    assert!(fact.contains("cfg.toml"), "{fact}");
    // The refused write is a fact on a verified contract, not an unverified
    // mark, and the attempt the orchestrator cancelled is not said to have
    // finished.
    assert!(report.unverified.is_empty(), "{report:?}");
    let refused = report
        .notices
        .iter()
        .find(|n| n.starts_with("contract a: verified by `test -f a.txt`;"))
        .unwrap_or_else(|| panic!("the refusal is kept as a fact: {report:?}"));
    assert!(
        refused.contains(
            "the worker was cancelled after a policy refused `apply_patch` on `cfg.toml`"
        ),
        "{refused}"
    );
    assert!(!script.saw_grant.load(Ordering::SeqCst), "nothing granted");
    assert_eq!(script.attempts.load(Ordering::SeqCst), 1, "one attempt");
    assert!(
        !permission_lines(&trace)
            .iter()
            .any(|l| l.contains("granted") && l.contains("cfg.toml")),
        "{trace}"
    );
}

/// The same report, with a check the in-scope result fails: the hold stands,
/// as a plan-shape error, and says what the check said. Nothing is merged,
/// and the attempt is not retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_shortfall_whose_in_scope_result_fails_its_check_stays_held() {
    let (report, trace, script) = run(
        Worker::WritesThenReportsUnowned,
        control(kgraf_tools::PermissionMode::Manual),
        "test -f never.txt",
        "sf-held-fails",
    )
    .await;
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}\n{trace}");
    };
    assert!(reason.contains("plan-shape error"), "{reason}");
    assert!(
        reason.contains("Its in-scope result was checked first and failed"),
        "{reason}"
    );
    assert!(reason.contains("test -f never.txt"), "{reason}");
    assert!(report.verified.is_empty(), "{report:?}");
    assert_eq!(script.attempts.load(Ordering::SeqCst), 1, "no retry");
    assert!(
        !report
            .notices
            .iter()
            .any(|n| n.contains("verified in its own scope")),
        "{report:?}"
    );
}

/// A check that checks nothing cannot overrule the shortfall: the hold
/// stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vacuous_check_does_not_overrule_a_held_shortfall() {
    let (report, trace, _) = run(
        Worker::WritesThenReportsUnowned,
        control(kgraf_tools::PermissionMode::Manual),
        "true",
        "sf-held-vacuous",
    )
    .await;
    let kgraf_orchestrator::RunStatus::Blocked { reason } = &report.status else {
        panic!("{report:?}\n{trace}");
    };
    assert!(reason.contains("plan-shape error"), "{reason}");
    assert!(reason.contains("checks nothing"), "{reason}");
    assert!(report.verified.is_empty(), "{report:?}");
}

/// Decision 0018, amended 2026-10-02, through a hosted run: the run check
/// needs both files of a dependent split, so it cannot pass on `a` alone. It
/// is deferred for `a` (said in a notice), `b` is checked with it whole once
/// `a` has merged, and the root gate runs it on the merged result before the
/// run completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_check_a_dependent_split_needs_is_deferred_and_judged_at_the_root_gate() {
    let check = "test -f a.txt && test -f b.txt";
    let (report, trace, _) = run(
        Worker::DependentSplit,
        control(kgraf_tools::PermissionMode::Manual),
        check,
        "verify-deferred",
    )
    .await;
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{trace}"
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("contract a:") && n.contains("deferred to the root gate")),
        "{report:?}"
    );
    assert!(
        !report
            .notices
            .iter()
            .any(|n| n.contains("contract b:") && n.contains("deferred")),
        "the last contract is checked whole: {report:?}"
    );
    let root_passed = trace.lines().any(|l| {
        l.contains(r#""event":"gate""#)
            && l.contains(r#""gate":"gate:root""#)
            && l.contains(r#""state":"passed""#)
    });
    assert!(root_passed, "the root gate ran the check\n{trace}");
}

/// A deferred check still has to pass: a run check no merged result can pass
/// never completes the run, though it was deferred for `a`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deferred_run_check_that_never_passes_does_not_complete_the_run() {
    let check = "test -f a.txt && test -f b.txt && test -f c.txt";
    let (report, trace, _) = run(
        Worker::DependentSplit,
        control(kgraf_tools::PermissionMode::Manual),
        check,
        "verify-deferred-fails",
    )
    .await;
    assert!(
        matches!(report.status, kgraf_orchestrator::RunStatus::Blocked { .. }),
        "{report:?}\n{trace}"
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("contract a:") && n.contains("deferred to the root gate")),
        "{report:?}"
    );
    assert!(
        !report.verified.contains(&"b".to_owned()),
        "b is checked whole and fails: {report:?}"
    );
}

/// Decision 0030, amended 2026-10-06, through a hosted run. Round 8
/// greenfield: the worker of the contract that writes the root manifest asked
/// for a member manifest a later contract writes, and was told "do not edit
/// it; your attempt is judged by its own acceptance check", which could not
/// pass. Here the plan passed intake against a checkout that still held the
/// member's manifest; the worker's report now holds the contract for a
/// re-plan, and nothing is granted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_manifest_a_later_contract_writes_holds_for_a_replan() {
    let (_dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = "test -f README.md".into();
    // These runs script a plan whose contracts wait on one another to test what
    // happens between contracts (shortfalls, deferred checks, merges); a straight
    // chain would be one contract (decision 0014, amendment of 2026-10-14).
    config.collapse_chains = false;
    let root = config.root.clone();
    std::fs::create_dir_all(root.join("cli")).expect("mkdir");
    std::fs::write(root.join("cli/Cargo.toml"), "[package]\nname = \"cli\"\n").expect("write");
    let script = Script::new(Worker::NeedsLaterMember);
    *script.root.lock().unwrap_or_else(|p| p.into_inner()) = Some(root.clone());
    let host = Host::open(config)
        .expect("open")
        .with_permissions(control(kgraf_tools::PermissionMode::Manual))
        .with_route_provider("mine", script.clone())
        .expect("a used route");
    let outcome = host
        .prepare(RunId::new("sf-member"), None, &mut |_| {})
        .await
        .expect("prepare")
        .run(RunStart::Instruction("build a two-crate workspace".into()))
        .await
        .expect("run");
    let trace =
        std::fs::read_to_string(root.join(".kgraf/runs/sf-member/trace.jsonl")).expect("trace");
    let lines = shortfall_lines(&trace);
    assert_eq!(lines.len(), 1, "{trace}");
    assert!(lines[0].contains("plan-shape error"), "{}", lines[0]);
    assert!(lines[0].contains("Re-plan"), "{}", lines[0]);
    assert!(lines[0].contains("`cli/Cargo.toml`"), "{}", lines[0]);
    assert!(lines[0].contains("cargo test -p core"), "{}", lines[0]);
    assert!(
        !lines[0].contains("judged by its own acceptance"),
        "{}",
        lines[0]
    );
    assert!(!script.saw_grant.load(Ordering::SeqCst), "nothing granted");
    assert!(
        !outcome.report.verified.contains(&"b".to_owned()),
        "{:?}",
        outcome.report
    );
}

/// The stack a hosted run needs on the thread that polls it, held under a
/// bound. Measured in a debug build on macOS: 1.83 MB before the run path
/// built its large futures on the heap (`kgraf_core::heap_future`), 0.45 MB
/// after. Rust's test threads and tokio's worker threads get 2 MiB; a Windows
/// build overflowed it in this scenario. The run goes on a thread with 1 MiB,
/// half of that: a change that brings back the old depth overflows here,
/// which ends the test binary with "has overflowed its stack" and fails the
/// run, as it would on Windows.
#[test]
fn a_hosted_run_fits_in_half_a_test_threads_stack() {
    const STACK: usize = 1024 * 1024;
    let outcome = std::thread::Builder::new()
        .name("hosted-run-1mib".into())
        .stack_size(STACK)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let allowing = control_with(
                    kgraf_tools::PermissionMode::Auto,
                    kgraf_tools::permissions::AllowSet::default()
                        .with_programs(["mkdir", "cp"])
                        .expect("allow"),
                    false,
                );
                let (report, trace, script) =
                    run(Worker::RunsScript, allowing, "test -f a.txt", "sf-stack").await;
                (
                    report.status,
                    format!("{trace}\n{}", script.failed_tool_results()),
                )
            })
        })
        .expect("spawn")
        .join()
        .expect("the run's thread");
    assert_eq!(
        outcome.0,
        kgraf_orchestrator::RunStatus::Completed,
        "{}",
        outcome.1
    );
}

/// The future of a prepared run is a pointer and a few fields, not the run's
/// whole state: a caller that awaits it, or spawns it, holds kilobytes, not
/// the 64 KB it held when the run's future was built inline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prepared_runs_future_is_small() {
    let (_dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = "test -f a.txt".into();
    let host = Host::open(config)
        .expect("open")
        .with_route_provider("mine", Script::new(Worker::Writes))
        .expect("a used route");
    let prepared = host
        .prepare(RunId::new("sf-size"), None, &mut |_| {})
        .await
        .expect("prepare");
    let running = prepared.run(RunStart::Instruction("copy README.md to a.txt".into()));
    let size = std::mem::size_of_val(&running);
    assert!(size < 4096, "the prepared run's future is {size} bytes");
}
