//! Decision 0084 through the real `assemble()` path: the worker
//! configuration's build and test allowlist and shell timeout reach the shell
//! tool an attempt actually gets, and the worker loop runs an allowlisted
//! build under a declared write scope while it refuses every other call that
//! may write anywhere. The model is fake; the worker loop, the approval chain
//! (unattended), the shell tool and the sandbox are real. A child of
//! `tests.rs`, for its fixtures.

use super::*;

/// One attempt of a fake model that makes one shell call with `argv`, in a
/// fresh worktree of `r`, under `config` and `write_scope`. Returns the call's
/// rendered output and whether it was an error.
async fn one_shell_call(
    r: &Repo,
    config: WorkerConfig,
    worker: &str,
    argv: serde_json::Value,
    write_scope: Option<Vec<PathBuf>>,
) -> (String, bool) {
    let state = r._dir.path().join(format!("state-{worker}"));
    std::fs::create_dir_all(&state).expect("state");
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let input = serde_json::json!({ "command": argv });
    let provider = || Arc::new(CallsOneTool("shell", input.clone()));
    let mut routes = BTreeMap::new();
    routes.insert(
        "fake:m".to_owned(),
        RouteParts {
            provider: provider(),
            vendor: "fake (fake)".into(),
            vendor_name: "fake".into(),
            model: "m".into(),
        },
    );
    let assembler = AttemptAssembler {
        root: r.root.clone(),
        state: state.clone(),
        graph_path: state.join("graph.db"),
        indexed: false,
        ledger: ledger.clone(),
        config,
        max_output_tokens: 1_000,
        read_denies: vec![state.clone()],
        workspaces: r.workspaces.clone(),
        routes,
        extra_tools: Vec::new(),
        session_line: None,
        prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
        machine_gate: test_machine_gate(),
        tbench_bridge_socket: None,
        dependency_refetch: crate::deps::inert_refetch(),
    };
    let context = ContextConfig::for_model(200_000).expect("context");
    let ws = worktree(r, worker).await;
    let route = Route {
        name: "fake:m".into(),
        vendor: "fake".into(),
        env: Arc::new(kgraf_runtime::WorkerEnv::new(
            ledger.clone(),
            provider(),
            Arc::new(kgraf_tools::ToolRegistry::new()),
            assemble_prefix(),
        )),
        context: context.clone(),
        readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
        local: false,
    };
    let built = assembler
        .build(&route, &ws, &worker.into(), &"r9/c-build".into())
        .await
        .expect("built");
    let brief = kgraf_runtime::WorkerBrief {
        task: "r9/c-build".into(),
        worker: worker.into(),
        instruction: "build it".into(),
        workspace_root: ws.clone(),
        model: "m".into(),
        slices: vec![],
        skills: vec![],
        step_budget: 4,
        context,
        write_scope,
    };
    let mut handle = kgraf_runtime::spawn_worker(
        built.env.clone(),
        brief,
        kgraf_runtime::SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut result = None;
    while let Some(ev) = handle.next_event().await {
        if let kgraf_runtime::WorkerEvent::ToolCallFinished {
            output, is_error, ..
        } = ev
        {
            result = Some((output, is_error));
        }
    }
    if let Some(closer) = built.closer {
        closer.close().await;
    }
    result.expect("the shell call finished")
}

fn scope() -> Option<Vec<PathBuf>> {
    Some(vec![PathBuf::from("src/lib.rs")])
}

/// Under a declared write scope, in an unattended attempt: `cargo check` (on
/// the default allowlist) runs in the sandbox and passes; `touch` is refused
/// with a message that names the allowlist; a smuggled `--config` is refused
/// naming the option. Before decision 0084 all three were refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn under_a_write_scope_only_the_allowlisted_build_runs() {
    let (r, _denies) = lockless_crate();
    let (out, is_error) = one_shell_call(
        &r,
        WorkerConfig::default(),
        "w-check",
        serde_json::json!(["cargo", "check", "--quiet"]),
        scope(),
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("exit status: 0"), "{out}");

    let (out, is_error) = one_shell_call(
        &r,
        WorkerConfig::default(),
        "w-touch",
        serde_json::json!(["touch", "src/lib.rs"]),
        scope(),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("build and test allowlist"), "{out}");

    let (out, is_error) = one_shell_call(
        &r,
        WorkerConfig::default(),
        "w-config",
        serde_json::json!([
            "cargo",
            "check",
            "--config",
            "build.rustc-wrapper='/bin/sh'"
        ]),
        scope(),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("--config"), "{out}");
}

/// A fake vendor key for a throwaway checkout's `.env`; never a real one.
const FAKE_DOTENV_KEY: &str = "sk-HOSTTESTKEY9f8e7d6c5b4a3210zyxwvQ";

/// Decision 0084: a checkout-root `.env` (the user's vendor keys) is
/// unreadable to a worker's build, which runs in a worktree inside that
/// checkout, and its values are registered with the scrubber when the
/// attempt is assembled, so they never reach a model even if printed. The
/// command is the run's own configured check, run as configured, reading the
/// file by its absolute path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkout_env_is_unreadable_from_a_worktree_build_and_scrubbed() {
    let (r, _denies) = lockless_crate();
    let dotenv = r.root.canonicalize().expect("root").join(".env");
    std::fs::write(&dotenv, format!("VENDOR_API_KEY={FAKE_DOTENV_KEY}\n")).expect("env");
    let check = format!("cat '{}'", dotenv.display());
    let config = WorkerConfig::default().with_configured_commands([check.as_str()]);
    let (out, is_error) = one_shell_call(
        &r,
        config,
        "w-env",
        serde_json::json!(["sh", "-c", check]),
        scope(),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(!out.contains(FAKE_DOTENV_KEY), "{out}");
    #[cfg(target_os = "macos")]
    assert!(out.contains("Operation not permitted"), "{out}");
    #[cfg(target_os = "linux")]
    assert!(out.contains("Permission denied"), "{out}");
    // Assembling the attempt registered the value.
    assert!(
        !kgraf_secrets::scrub_model_facing(&format!("printed: {FAKE_DOTENV_KEY}"))
            .contains(FAKE_DOTENV_KEY)
    );
}

/// An empty `build_commands` in the configuration turns the allowlist off:
/// the same `cargo check` is refused under the scope again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_build_commands_list_allows_nothing() {
    let (r, _denies) = lockless_crate();
    let config =
        WorkerConfig::parse(Path::new("worker.json"), r#"{"build_commands": []}"#).expect("config");
    let (out, is_error) = one_shell_call(
        &r,
        config,
        "w-none",
        serde_json::json!(["cargo", "check", "--quiet"]),
        scope(),
    )
    .await;
    assert!(is_error, "{out}");
    assert!(!out.contains("exit status: 0"), "{out}");
}

/// The configuration's shell timeout is the one the attempt's shell uses: at
/// 1 s, a configured command that sleeps 5 s is killed, its output so far
/// kept under the marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configured_shell_timeout_reaches_the_attempts_shell() {
    let r = repo();
    let config = WorkerConfig::default()
        .with_configured_commands(["echo started && sleep 5"])
        .with_shell_timeout(Duration::from_secs(1));
    let (out, is_error) = one_shell_call(
        &r,
        config,
        "w-short",
        serde_json::json!(["sh", "-c", "echo started && sleep 5"]),
        None,
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("[killed after 1 s]"), "{out}");
    assert!(out.contains("started"), "{out}");
}

/// The default shell timeout, through `assemble()`: a command longer than
/// the old 30 s default runs to completion. Takes about 31 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_default_timeout_a_command_longer_than_30_s_survives() {
    let r = repo();
    let config = WorkerConfig::default().with_configured_commands(["sleep 31"]);
    let (out, is_error) = one_shell_call(
        &r,
        config,
        "w-long",
        serde_json::json!(["sleep", "31"]),
        None,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("exit status: 0"), "{out}");
    assert!(!out.contains("killed after"), "{out}");
}
