//! Decision 0094's protected writes through a real hosted run: in Bypass, a
//! worker's write to `.git` and to kGraf's settings lands, is checked and is
//! recorded; a write that corrupts a ref or a settings file is restored, the
//! attempt fails with the check's output and nothing of it is merged; the
//! ledger and the trace stay refused in Bypass; another mode refuses every
//! protected write. The model is a script; the host, the orchestrator, the
//! worker loop, the approval chain, the tools and the sandbox are real. A
//! child of `tests.rs`, for its fixtures.

use super::*;
use kgraf_provider::{Content, Role, StopReason};
use std::sync::Mutex as StdMutex;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Worker {
    /// Adds a line to `.git/info/exclude`, writes a valid `.kgraf/worker.json`,
    /// then writes `a.txt`.
    Good,
    /// Writes garbage into `refs/heads/main` with a shell command, then
    /// writes `a.txt`. Run on macOS only (see its test).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    CorruptsRef,
    /// Writes a `.kgraf/worker.json` the host would refuse to load, then
    /// writes `a.txt`.
    BadSettings,
    /// Tries the ledger and the trace, then writes `a.txt`.
    Books,
    /// Deletes the object of its `HEAD` commit and every pack with a shell
    /// git command, then writes `a.txt`. Run on macOS only.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    DeletesObjects,
}

struct Script {
    worker: Worker,
    /// Every worker request's messages, as text.
    seen: StdMutex<Vec<String>>,
}

fn tool(name: &str, input: serde_json::Value, round: usize) -> Content {
    Content::ToolUse {
        id: format!("call-{round}"),
        name: name.into(),
        input,
        signature: None,
    }
}

fn patch(body: &str) -> serde_json::Value {
    serde_json::json!({ "patch": format!("*** Begin Patch\n{body}\n*** End Patch") })
}

fn write_a() -> Content {
    tool("apply_patch", patch("*** Add File: a.txt\n+a"), 9)
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
                    r#"{"contracts":[{"id":"a","instruction":"write a.txt","acceptance":"a.txt exists","writes":[{"path":"a.txt"}],"tools":[]}]}"#,
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
            self.seen
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(format!("{:?}", request.messages));
            let call = match (self.worker, round) {
                (Worker::Good, 0) => Some(tool(
                    "apply_patch",
                    patch("*** Update File: .git/info/exclude\n@@\n /.kgraf.migrating\n+/scratch/"),
                    round,
                )),
                (Worker::Good, 1) => Some(tool(
                    "apply_patch",
                    patch("*** Add File: .kgraf/worker.json\n+{\"shell_timeout_secs\": 300}"),
                    round,
                )),
                (Worker::Good, 2) => Some(write_a()),
                (Worker::CorruptsRef, 0) => Some(tool(
                    "shell",
                    serde_json::json!({"command": [
                        "sh", "-c",
                        "echo garbage > \"$(git rev-parse --git-path refs/heads/main)\""
                    ]}),
                    round,
                )),
                (Worker::CorruptsRef, 1) => Some(write_a()),
                (Worker::DeletesObjects, 0) => Some(tool(
                    "shell",
                    serde_json::json!({"command": [
                        "sh", "-c",
                        "o=\"$(git rev-parse --git-path objects)\"; h=\"$(git rev-parse HEAD)\"; \
                         rm -f \"$o/$(echo $h | cut -c1-2)/$(echo $h | cut -c3-)\" \"$o\"/pack/*"
                    ]}),
                    round,
                )),
                (Worker::DeletesObjects, 1) => Some(write_a()),
                (Worker::BadSettings, 0) => Some(tool(
                    "apply_patch",
                    patch("*** Add File: .kgraf/worker.json\n+{\"shell_timeout_secs\": 0}"),
                    round,
                )),
                (Worker::BadSettings, 1) => Some(write_a()),
                (Worker::Books, 0) => Some(tool(
                    "apply_patch",
                    patch("*** Add File: .kgraf/ledger.db\n+truncated"),
                    round,
                )),
                (Worker::Books, 1) => Some(tool(
                    "apply_patch",
                    patch("*** Add File: .kgraf/runs/pw-books/trace.jsonl\n+{}"),
                    round,
                )),
                (Worker::Books, 2) => Some(write_a()),
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
    Arc::new(
        kgraf_tools::PermissionControl::new(
            kgraf_tools::PermissionSettings {
                mode,
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

async fn run(worker: Worker, mode: kgraf_tools::PermissionMode, id: &str) -> Ran {
    let (dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = "test -f a.txt".into();
    let root = config.root.clone();
    let script = Arc::new(Script {
        worker,
        seen: StdMutex::new(Vec::new()),
    });
    let host = Host::open(config)
        .expect("open")
        .with_permissions(control(mode))
        .with_route_provider("mine", script.clone())
        .expect("a used route");
    let outcome = host
        .prepare(RunId::new(id), None, &mut |_| {})
        .await
        .expect("prepare")
        .run(RunStart::Instruction("write a.txt".into()))
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

fn protected_lines(trace: &str) -> Vec<&str> {
    trace
        .lines()
        .filter(|l| l.contains(r#""event":"permission""#) && l.contains("protected_write"))
        .collect()
}

fn git_ok(root: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git")
        .status
        .success()
}

fn exclude(root: &Path) -> String {
    std::fs::read_to_string(root.join(".git/info/exclude")).expect("exclude")
}

/// Bypass: the worker's line lands in the shared repository's exclude file,
/// its settings file lands in the state directory, each write is traced as
/// kept, and the contract's own work is verified and merged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_writes_to_git_and_settings_land_and_are_recorded() {
    let r = run(Worker::Good, kgraf_tools::PermissionMode::Bypass, "pw-good").await;
    assert_eq!(
        r.report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{:?}\n{}",
        r.report,
        r.trace
    );
    assert_eq!(r.report.verified, vec!["a".to_owned()]);
    assert!(
        exclude(&r.root).ends_with("/scratch/\n"),
        "{}",
        exclude(&r.root)
    );
    assert_eq!(
        std::fs::read_to_string(r.root.join(".kgraf/worker.json")).expect("settings"),
        "{\"shell_timeout_secs\": 300}\n"
    );
    let lines = protected_lines(&r.trace);
    assert_eq!(lines.len(), 2, "{}", r.trace);
    // The trace names each path in the platform's own form, JSON-escaped:
    // `.git\\info\\exclude` on Windows, which a hosted Windows self-test
    // met as a failure of this assertion.
    let slashed: Vec<String> = lines
        .iter()
        .map(|l| l.replace("\\\\", "/").replace('\\', "/"))
        .collect();
    assert!(
        slashed[0].contains(".git/info/exclude") && slashed[0].contains("kept"),
        "{lines:?}"
    );
    assert!(
        slashed[1].contains(".kgraf/worker.json") && slashed[1].contains("kept"),
        "{lines:?}"
    );
    assert!(lines.iter().all(|l| l.contains("Bypass")), "{lines:?}");
    assert!(git_ok(
        &r.root,
        &["fsck", "--connectivity-only", "--no-dangling"]
    ));
    assert!(
        git_ok(&r.root, &["cat-file", "-e", "kgraf/pw-good:a.txt"]),
        "a.txt merged into the integration branch"
    );
}

/// Bypass: a shell command that writes garbage into `refs/heads/main` is
/// caught when it ends; the ref is restored, every attempt fails with the
/// check's output, nothing is merged, and the repository passes fsck.
// A shell command writes the repository directory only where the sandbox
// can keep its configuration files read-only (macOS); elsewhere it runs
// read-only and this corruption cannot happen.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_restores_a_corrupted_ref_and_fails_the_attempt() {
    let r = run(
        Worker::CorruptsRef,
        kgraf_tools::PermissionMode::Bypass,
        "pw-ref",
    )
    .await;
    assert_ne!(
        r.report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{}",
        r.trace
    );
    assert!(r.report.verified.is_empty());
    let failed: Vec<&str> = r
        .trace
        .lines()
        .filter(|l| l.contains(r#""event":"worker_finished""#))
        .collect();
    assert!(!failed.is_empty(), "{}", r.trace);
    assert!(
        failed
            .iter()
            .all(|l| l.contains(r#""outcome":"failed""#) && l.contains("integrity check")),
        "{failed:?}"
    );
    let lines = protected_lines(&r.trace);
    assert!(
        !lines.is_empty() && lines.iter().all(|l| l.contains("restored")),
        "{lines:?}"
    );
    assert!(git_ok(
        &r.root,
        &["fsck", "--connectivity-only", "--no-dangling"]
    ));
    assert!(git_ok(
        &r.root,
        &["rev-parse", "--verify", "refs/heads/main"]
    ));
    assert!(!git_ok(&r.root, &["cat-file", "-e", "kgraf/pw-ref:a.txt"]));
}

/// Bypass: a shell git command that deletes its HEAD commit's object (and
/// any pack) is caught when it ends; the object is restored from the
/// attempt's object snapshot, every attempt fails with the check's output,
/// nothing is merged, the repository passes fsck, and no snapshot is left
/// in the state directory.
// macOS only, as above.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_restores_deleted_objects_and_fails_the_attempt() {
    let r = run(
        Worker::DeletesObjects,
        kgraf_tools::PermissionMode::Bypass,
        "pw-objects",
    )
    .await;
    assert_ne!(
        r.report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{}",
        r.trace
    );
    assert!(r.report.verified.is_empty());
    let failed: Vec<&str> = r
        .trace
        .lines()
        .filter(|l| l.contains(r#""event":"worker_finished""#))
        .collect();
    assert!(!failed.is_empty(), "{}", r.trace);
    assert!(
        failed
            .iter()
            .all(|l| l.contains(r#""outcome":"failed""#) && l.contains("object files restored")),
        "{failed:?}"
    );
    let lines = protected_lines(&r.trace);
    assert!(
        !lines.is_empty()
            && lines
                .iter()
                .all(|l| l.contains(".git/objects") && l.contains("restored")),
        "{lines:?}"
    );
    assert!(git_ok(
        &r.root,
        &["fsck", "--connectivity-only", "--no-dangling"]
    ));
    assert!(!git_ok(
        &r.root,
        &["cat-file", "-e", "kgraf/pw-objects:a.txt"]
    ));
    let left: Vec<_> = std::fs::read_dir(
        r.root
            .join(".kgraf")
            .join(kgraf_tools::protected::SNAPSHOT_DIR),
    )
    .map(|d| d.flatten().map(|e| e.path()).collect())
    .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
}

/// Bypass: a settings file the host would refuse to load is put back (here
/// removed, as it did not exist) and the attempt fails with the parser's
/// words.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_restores_a_settings_file_that_does_not_load() {
    let r = run(
        Worker::BadSettings,
        kgraf_tools::PermissionMode::Bypass,
        "pw-settings",
    )
    .await;
    assert_ne!(
        r.report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{}",
        r.trace
    );
    assert!(!r.root.join(".kgraf/worker.json").exists());
    assert!(
        r.trace.contains("shell_timeout_secs") || r.trace.contains("timeout"),
        "the parser's reason is in the failure: {}",
        r.trace
    );
    assert!(
        protected_lines(&r.trace)
            .iter()
            .all(|l| l.contains("restored"))
    );
}

/// Bypass does not open kGraf's books: the ledger and the trace are refused
/// with the reason, and stay intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_refuses_the_ledger_and_the_trace() {
    let r = run(
        Worker::Books,
        kgraf_tools::PermissionMode::Bypass,
        "pw-books",
    )
    .await;
    let seen = r.script.seen.lock().expect("seen").join("\n");
    assert!(seen.contains("books"), "{seen}");
    let ledger = std::fs::read(r.root.join(".kgraf/ledger.db")).expect("ledger");
    assert!(
        ledger.starts_with(b"SQLite format 3\0"),
        "the ledger is a database still"
    );
    assert!(
        r.trace
            .lines()
            .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()),
        "the trace is intact"
    );
    assert!(protected_lines(&r.trace).is_empty());
}

/// Accept edits grants file edits, yet every protected write stays refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_mode_refuses_every_protected_write() {
    let r = run(
        Worker::Good,
        kgraf_tools::PermissionMode::AcceptEdits,
        "pw-accept",
    )
    .await;
    assert!(!exclude(&r.root).contains("/scratch/"));
    assert!(!r.root.join(".kgraf/worker.json").exists());
    let seen = r.script.seen.lock().expect("seen").join("\n");
    assert!(
        seen.contains("steers git") && seen.contains("kGraf's own state"),
        "{seen}"
    );
    assert!(protected_lines(&r.trace).is_empty());
}
