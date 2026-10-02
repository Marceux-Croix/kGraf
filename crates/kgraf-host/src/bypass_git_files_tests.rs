//! Decision 0094's tracked git files through a real hosted run: in Bypass, a
//! worker's writes to `.gitattributes`, `.gitmodules` and `.githooks/...` are
//! ordinary files under the declared write scope, verified and merged as
//! plain data; undeclared, they are out of scope like any file; in another
//! mode they are refused. The repository is armed (a filter, a merge driver
//! and a hook that would leave a marker), and kGraf's own git runs none of
//! them. The model is a script; the host, the orchestrator, the worker loop,
//! the approval chain, the tools and the sandbox are real. A child of
//! `tests.rs`, for its fixtures.

use super::*;
use kgraf_provider::{Content, Role, StopReason};
use std::sync::Mutex as StdMutex;

const GIT_FILES: [&str; 3] = [".gitattributes", ".gitmodules", ".githooks/pre-commit"];

struct Script {
    /// Whether the plan declares the three git files beside `a.txt`.
    declare: bool,
    seen: StdMutex<Vec<String>>,
}

fn patch(body: &str) -> serde_json::Value {
    serde_json::json!({ "patch": format!("*** Begin Patch\n{body}\n*** End Patch") })
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
            let mut writes = vec![r#"{"path":"a.txt"}"#.to_owned()];
            if self.declare {
                writes.extend(GIT_FILES.iter().map(|p| format!(r#"{{"path":"{p}"}}"#)));
            }
            (
                vec![Content::text(format!(
                    r#"{{"contracts":[{{"id":"a","instruction":"write the files","acceptance":"a.txt exists","writes":[{}],"tools":[]}}]}}"#,
                    writes.join(",")
                ))],
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
            let call = match round {
                // The git files in one patch with an ordinary, declared file.
                0 => Some(patch(
                    "*** Add File: .gitattributes\n+*.txt filter=evil merge=evil\n\
                     *** Add File: .gitmodules\n+[submodule \"s\"]\n+\tpath = s\n+\turl = ../nowhere\n\
                     *** Add File: .githooks/pre-commit\n+#!/bin/sh\n+exit 0\n\
                     *** Add File: a.txt\n+a",
                )),
                // Refused above when undeclared: the ordinary file alone.
                1 if !self.declare => Some(patch("*** Add File: a.txt\n+a")),
                _ => None,
            };
            match call {
                Some(input) => (
                    vec![Content::ToolUse {
                        id: format!("call-{round}"),
                        name: "apply_patch".into(),
                        input,
                        signature: None,
                    }],
                    StopReason::ToolUse,
                ),
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
    seen: String,
    root: PathBuf,
    marks: PathBuf,
    _dir: tempfile::TempDir,
}

fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn run(declare: bool, mode: kgraf_tools::PermissionMode, id: &str) -> Ran {
    let (dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = "test -f a.txt".into();
    let root = config.root.clone();
    // Armed: a filter and a merge driver named `evil`, and a hooks path, each
    // leaving a marker if kGraf's own git ran it.
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks).expect("marks");
    let mark = |name: &str| format!("touch '{}'/{name}; cat", marks.display());
    for (k, v) in [
        ("filter.evil.clean", mark("clean")),
        ("filter.evil.smudge", mark("smudge")),
        (
            "merge.evil.driver",
            format!("{} >/dev/null; exit 0", mark("merge")),
        ),
        ("hook.planted.command", mark("confighook")),
        ("hook.planted.event", "reference-transaction".to_owned()),
    ] {
        assert!(git_out(&root, &["config", k, &v]).is_some(), "{k}");
    }
    let script = Arc::new(Script {
        declare,
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
        .run(RunStart::Instruction("write the files".into()))
        .await
        .expect("run");
    let trace = std::fs::read_to_string(root.join(".kgraf/runs").join(id).join("trace.jsonl"))
        .expect("trace");
    let seen = script.seen.lock().expect("seen").join("\n");
    Ran {
        report: outcome.report,
        trace,
        seen,
        root,
        marks,
        _dir: dir,
    }
}

fn marks(r: &Ran) -> Vec<String> {
    std::fs::read_dir(&r.marks)
        .expect("marks")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect()
}

/// Bypass, declared: the three files and the ordinary ones land on the
/// integration branch byte for byte, and no planted program ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_merges_declared_git_files_as_plain_data() {
    let r = run(true, kgraf_tools::PermissionMode::Bypass, "gf-bypass").await;
    assert_eq!(
        r.report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "{:?}\n{}",
        r.report,
        r.trace
    );
    let branch = "kgraf/gf-bypass";
    for (path, text) in [
        (".gitattributes", "*.txt filter=evil merge=evil\n"),
        (
            ".gitmodules",
            "[submodule \"s\"]\n\tpath = s\n\turl = ../nowhere\n",
        ),
        (".githooks/pre-commit", "#!/bin/sh\nexit 0\n"),
        ("a.txt", "a\n"),
    ] {
        assert_eq!(
            git_out(&r.root, &["cat-file", "blob", &format!("{branch}:{path}")]).as_deref(),
            Some(text),
            "{path}\n{}",
            r.trace
        );
    }
    assert_eq!(marks(&r), Vec::<String>::new(), "a planted program ran");
}

/// Bypass, undeclared: the git files are out of scope like any file and
/// never reach the branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bypass_holds_undeclared_git_files_to_the_scope() {
    let r = run(false, kgraf_tools::PermissionMode::Bypass, "gf-scope").await;
    assert!(
        r.seen
            .contains(".gitattributes is outside the declared write scope"),
        "{}",
        r.seen
    );
    assert!(
        git_out(
            &r.root,
            &["cat-file", "-e", "kgraf/gf-scope:.gitattributes"]
        )
        .is_none()
    );
    assert_eq!(marks(&r), Vec::<String>::new(), "a planted program ran");
}

/// Another mode refuses the git files even when the worker tries them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_mode_refuses_the_git_files() {
    let r = run(false, kgraf_tools::PermissionMode::AcceptEdits, "gf-accept").await;
    assert!(r.seen.contains("steers git"), "{}", r.seen);
    assert!(
        git_out(
            &r.root,
            &["cat-file", "-e", "kgraf/gf-accept:.gitattributes"]
        )
        .is_none()
    );
    assert!(!r.root.join(".gitattributes").exists());
}
