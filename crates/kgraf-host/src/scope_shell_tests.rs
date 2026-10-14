//! Through a real hosted run: files an attempt's shell commands wrote outside
//! its contract's declared writes are put to the user's permission mode, as a
//! write tool's would be (decision 0094): Bypass grants them, records the
//! grant and merges; a mode that would not grant a write tool's path fails the
//! attempt saying why, and a second attempt that fails on the same paths holds
//! the contract at once. A path another contract declares is never granted.
//! Processes a command left running end when its attempt ends. The model is a
//! script; the host, the orchestrator, the worker loop, the approval chain,
//! the shell tool and the sandbox are real. A child of `tests.rs`, for its
//! fixtures.

use super::*;
use kgraf_provider::{Content, Role, StopReason};
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the scripted worker does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Worker {
    /// Contract `a` declares no writes. Its worker builds the package
    /// directory and copies a wheel there with one shell command, as the
    /// instruction says, then answers.
    BuildsPackages,
    /// Contract `a` declares no writes; its command writes `b.txt`, which
    /// contract `b` declares.
    WritesSibling,
    /// Contract `a` writes `a.txt`. Its worker starts a long process in the
    /// background with a shell command that returns at once, writes its
    /// file, and answers.
    #[cfg(target_os = "macos")]
    StartsServer,
}

struct Script {
    worker: Worker,
    /// Worker attempts started (round-0 requests of contract `a`).
    attempts: AtomicUsize,
    /// The background process id `StartsServer`'s command printed.
    server: std::sync::Mutex<Option<u32>>,
}

/// Contract `src` writes the package source; contract `a`, after it, declares
/// no writes while its instruction asks for `packages/` and a wheel copied
/// there.
const PACKAGES_PLAN: &str = r#"{"contracts":[{"id":"src","instruction":"TASK-SRC: write the package","acceptance":"the package exists","writes":[{"path":"vectorops/__init__.py"}],"tools":[]},{"id":"a","instruction":"TASK-A: create packages/ and copy the wheel there","acceptance":"the wheel is in packages/","writes":[],"tools":[],"depends_on":["src"]}]}"#;

const SIBLING_PLAN: &str = r#"{"contracts":[{"id":"a","instruction":"TASK-A: prepare","acceptance":"done","writes":[],"tools":[]},{"id":"b","instruction":"TASK-B: write b.txt","acceptance":"b.txt exists","writes":[{"path":"b.txt"}],"tools":[]}]}"#;

#[cfg(target_os = "macos")]
const SERVER_PLAN: &str = r#"{"contracts":[{"id":"a","instruction":"TASK-A: start the server and write a.txt","acceptance":"a.txt exists","writes":[{"path":"a.txt"}],"tools":[]}]}"#;

fn shell(command: &str, round: usize) -> Content {
    Content::ToolUse {
        id: format!("call-{round}"),
        name: "shell".into(),
        input: serde_json::json!({"command": ["sh", "-c", command]}),
        signature: None,
    }
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
            let plan = match self.worker {
                Worker::BuildsPackages => PACKAGES_PLAN,
                Worker::WritesSibling => SIBLING_PLAN,
                #[cfg(target_os = "macos")]
                Worker::StartsServer => SERVER_PLAN,
            };
            (vec![Content::text(plan)], StopReason::EndTurn)
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
            let task_a = seen.contains("TASK-A:");
            if round == 0 && task_a {
                self.attempts.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(at) = seen.rfind("SERVER=") {
                let digits: String = seen[at + 7..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(pid) = digits.parse() {
                    *self.server.lock().unwrap_or_else(|p| p.into_inner()) = Some(pid);
                }
            }
            let call = match (self.worker, task_a, round) {
                (Worker::BuildsPackages, false, 0) => Some(Content::ToolUse {
                    id: "call-0".into(),
                    name: "apply_patch".into(),
                    input: serde_json::json!({
                        "patch": "*** Begin Patch\n*** Add File: vectorops/__init__.py\n+def dotproduct(a, b): return 0\n*** End Patch"
                    }),
                    signature: None,
                }),
                (Worker::BuildsPackages, true, 0) => Some(shell(
                    "mkdir -p packages && cp README.md packages/vectorops-0.1.0-py3-none-any.whl",
                    round,
                )),
                (Worker::WritesSibling, true, 0) => Some(shell("cp README.md b.txt", round)),
                (Worker::WritesSibling, false, 0) => Some(Content::ToolUse {
                    id: "call-0".into(),
                    name: "apply_patch".into(),
                    input: serde_json::json!({
                        "patch": "*** Begin Patch\n*** Add File: b.txt\n+b\n*** End Patch"
                    }),
                    signature: None,
                }),
                #[cfg(target_os = "macos")]
                (Worker::StartsServer, true, 0) => Some(shell(
                    "perl -e 'setpgrp(0, 0); sleep 300' > /dev/null 2>&1 & sleep 1; echo SERVER=$!",
                    round,
                )),
                #[cfg(target_os = "macos")]
                (Worker::StartsServer, true, 1) => Some(Content::ToolUse {
                    id: "call-1".into(),
                    name: "apply_patch".into(),
                    input: serde_json::json!({
                        "patch": "*** Begin Patch\n*** Add File: a.txt\n+a\n*** End Patch"
                    }),
                    signature: None,
                }),
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

fn control(
    mode: kgraf_tools::PermissionMode,
    programs: &[&str],
) -> Arc<kgraf_tools::PermissionControl> {
    Arc::new(
        kgraf_tools::PermissionControl::new(
            kgraf_tools::PermissionSettings {
                mode,
                allow: kgraf_tools::permissions::AllowSet::default()
                    .with_programs(programs.iter().copied())
                    .expect("allow"),
                ..kgraf_tools::PermissionSettings::default()
            },
            kgraf_tools::ModeSource::CommandLine,
            kgraf_tools::permissions::ProcessFacts::default(),
        )
        .expect("control"),
    )
}

struct Ran {
    report: kgraf_orchestrator::RunReport,
    trace: String,
    script: Arc<Script>,
    root: PathBuf,
    _dir: tempfile::TempDir,
}

/// One hosted run to its end, nobody attending, three retries allowed.
async fn run(
    worker: Worker,
    permissions: Arc<kgraf_tools::PermissionControl>,
    verify: &str,
    id: &str,
) -> Ran {
    let (dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = verify.into();
    // These runs script a plan whose contracts wait on one another to test what
    // happens between contracts (shortfalls, deferred checks, merges); a straight
    // chain would be one contract (decision 0014, amendment of 2026-10-14).
    config.collapse_chains = false;
    let root = config.root.clone();
    let script = Arc::new(Script {
        worker,
        attempts: AtomicUsize::new(0),
        server: std::sync::Mutex::new(None),
    });
    let host = Host::open(config)
        .expect("open")
        .with_permissions(permissions)
        .with_max_retries(3)
        .with_route_provider("mine", script.clone())
        .expect("a used route");
    let outcome = host
        .prepare(RunId::new(id), None, &mut |_| {})
        .await
        .expect("prepare")
        .run(RunStart::Instruction("build and serve the package".into()))
        .await
        .expect("run");
    let trace = std::fs::read_to_string(root.join(".kgraf/runs").join(id).join("trace.jsonl"))
        .expect("trace");
    Ran {
        report: outcome.report,
        trace,
        script,
        root,
        _dir: dir,
    }
}

fn held_reason<'a>(report: &'a kgraf_orchestrator::RunReport, id: &str) -> &'a str {
    report
        .held
        .iter()
        .find(|(c, _)| c == id)
        .map(|(_, r)| r.as_str())
        .unwrap_or_else(|| panic!("contract {id} is not held: {report:?}"))
}

/// Bypass: a contract that declares no writes, whose command writes the
/// package directory, is granted those paths, verified on them and merged.
/// The grant is a `permission` event naming the path, and a notice says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_grants_files_a_command_wrote_outside_the_declared_writes() {
    let wheel = "packages/vectorops-0.1.0-py3-none-any.whl";
    let ran = run(
        Worker::BuildsPackages,
        control(kgraf_tools::PermissionMode::Bypass, &[]),
        &format!("test -f {wheel}"),
        "sw-bypass",
    )
    .await;
    let (report, trace) = (&ran.report, &ran.trace);
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{}",
        failed_verdicts(trace)
    );
    assert!(report.verified.contains(&"a".to_owned()), "{report:?}");
    assert_eq!(ran.script.attempts.load(Ordering::SeqCst), 1);
    assert!(
        trace.lines().any(|l| l.contains(r#""event":"permission""#)
            && l.contains(r#""what":"granted""#)
            && l.contains(wheel)
            && l.contains("shell commands")),
        "the grant is recorded\n{trace}"
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains(wheel) && n.contains("Bypass permission mode grants")),
        "{:?}",
        report.notices
    );
    // Merged: the file is on the run's integration branch.
    let shown = std::process::Command::new("git")
        .args(["show", &format!("kgraf/sw-bypass:{wheel}")])
        .current_dir(&ran.root)
        .output()
        .expect("git");
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
}

/// Auto, with the command's programs allowed but no path: the mode would not
/// grant a write tool's path outside the scope, so the attempt fails and says
/// why, and the second attempt, failing on the same path, holds the contract
/// at once instead of spending the two retries left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mode_that_refuses_the_path_fails_loud_and_a_repeat_holds_at_once() {
    let wheel = "packages/vectorops-0.1.0-py3-none-any.whl";
    let ran = run(
        Worker::BuildsPackages,
        control(kgraf_tools::PermissionMode::Auto, &["mkdir", "cp"]),
        &format!("test -f {wheel}"),
        "sw-auto",
    )
    .await;
    let (report, trace) = (&ran.report, &ran.trace);
    assert!(
        matches!(report.status, kgraf_orchestrator::RunStatus::Blocked { .. }),
        "{report:?}"
    );
    let verdicts = failed_verdicts(trace);
    assert!(
        verdicts.contains("wrote outside the declared scope")
            && verdicts.contains("The Auto permission mode does not grant this write")
            && verdicts.contains(wheel),
        "{verdicts}"
    );
    assert_eq!(
        ran.script.attempts.load(Ordering::SeqCst),
        2,
        "held on the repeat, not after max_retries\n{trace}"
    );
    let reason = held_reason(report, "a");
    assert!(reason.contains("same cause"), "{reason}");
    assert!(reason.contains(wheel), "{reason}");
    assert!(
        report.notices.iter().any(|n| n.contains("held at once")),
        "{:?}",
        report.notices
    );
    assert!(
        !trace
            .lines()
            .any(|l| l.contains(r#""what":"granted""#) && l.contains(wheel)),
        "never granted\n{trace}"
    );
}

/// Bypass never grants a path another contract declares (disjoint writes):
/// the attempt fails naming the owner, and the repeat holds at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_never_grants_a_path_another_contract_declares() {
    let ran = run(
        Worker::WritesSibling,
        control(kgraf_tools::PermissionMode::Bypass, &[]),
        "true",
        "sw-sibling",
    )
    .await;
    let (report, trace) = (&ran.report, &ran.trace);
    let verdicts = failed_verdicts(trace);
    assert!(
        verdicts.contains("`b.txt` belongs to contract `b`"),
        "{verdicts}\n{trace}"
    );
    assert!(
        !trace.contains("writes to `b.txt`"),
        "never granted\n{trace}"
    );
    assert_eq!(ran.script.attempts.load(Ordering::SeqCst), 2, "{trace}");
    let reason = held_reason(report, "a");
    assert!(reason.contains("same cause"), "{reason}");
    assert!(report.verified.contains(&"b".to_owned()), "{report:?}");
}

/// A command that started a process in the background, which moved to its
/// own process group, and returned: the shell's group kill at the command's
/// end does not reach it, so it runs on. The attempt's end finds it by its
/// session and ends it, and a notice and the trace say so. macOS only: on
/// Linux with PID namespaces the kernel ends such a process with its
/// command, and the id the command prints is the namespace's own.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_a_command_left_running_ends_with_its_attempt() {
    let ran = run(
        Worker::StartsServer,
        control(kgraf_tools::PermissionMode::Bypass, &[]),
        "test -f a.txt",
        "sw-server",
    )
    .await;
    let (report, trace) = (&ran.report, &ran.trace);
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{report:?}\n{}",
        failed_verdicts(trace)
    );
    let pid = ran
        .script
        .server
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .unwrap_or_else(|| panic!("the command printed no background process id\n{trace}"));
    let gone = || {
        // SAFETY: signal 0 checks existence only; it sends nothing.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc != 0
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !gone() {
        if std::time::Instant::now() >= deadline {
            // Not left behind by a failing test.
            // SAFETY: kill only sends a signal to the process the run started.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            panic!("process {pid} outlived its attempt\n{trace}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("left processes running")
                && n.contains(&pid.to_string())
                && n.contains("its worker finished")),
        "{:?}",
        report.notices
    );
    assert!(
        trace.contains("left processes running"),
        "the trace says so\n{trace}"
    );
}
