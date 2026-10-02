//! Decision 0074 end to end: two contracts in flight at once over real git
//! worktrees, the real verifier, merge-back and sandboxed validation runs, with
//! the loop's own speculation rounds. A conflicting pair is reported before
//! either lands; a clean pair lands while speculation is still validating it
//! (never delayed); nothing speculative reaches the integration branch.

use super::subagent_e2e::{MODEL, Script, WriteFile, call, git};
use crate::gates::CommandGateRunner;
use crate::verifier::CommandVerifier;
use crate::workspaces::{GitWorkspaces, ensure_integration_branch};
use kgraf_core::{ModelPrice, PriceTable, RunId};
use kgraf_graph::GraphStore;
use kgraf_orchestrator::{
    Admission, AlwaysReady, ConflictKind, GateRequest, GateRunner, GateVerdict, Limits,
    ModelCaller, ModelDecomposer, ModelGrounder, Orchestrator, OrchestratorConfig, Route,
    RunStateRecord, RunStatus, RunStore, SPECULATION_GATE, Services, SessionLine, SystemClock,
    TraceEvent, TraceLog, UntrustedText, session_tools,
};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{Content, Ledger};
use kgraf_runtime::{
    CallMode, ContextConfig, NeutralPrefixEncoder, PrefixRegistry, PrefixSnapshot, SharedPrefix,
    WorkerEnv,
};
use kgraf_sandbox::{MergeBack, WorkspaceManager};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// A worker's idle round while the test holds both contracts in flight.
struct Nap;

#[async_trait::async_trait]
impl Tool for Nap {
    fn name(&self) -> &str {
        "nap"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "nap".into(),
            description: "Wait".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(ToolOutput::text("rested"))
    }
}

/// The real sandboxed runner for speculative validation, held `delay` longer
/// and timed; quality gates pass untouched (they are not under test here).
struct Probe {
    inner: CommandGateRunner,
    delay: Duration,
    started_ms: Mutex<Option<i64>>,
    finished_ms: Mutex<Option<i64>>,
}

#[async_trait::async_trait]
impl GateRunner for Probe {
    fn commands(&self) -> Vec<String> {
        self.inner.commands()
    }
    async fn run(&self, request: GateRequest) -> Result<GateVerdict, String> {
        if request.gate != SPECULATION_GATE {
            return Ok(GateVerdict {
                passed: true,
                by: "not under test".into(),
                output: UntrustedText::new(""),
            });
        }
        self.started_ms
            .lock()
            .expect("lock")
            .get_or_insert(now_ms());
        tokio::time::sleep(self.delay).await;
        let verdict = self.inner.run(request).await;
        *self.finished_ms.lock().expect("lock") = Some(now_ms());
        verdict
    }
}

struct Outcome {
    trace: Vec<TraceEvent>,
    record: RunStateRecord,
    branch_log: String,
    probe: Arc<Probe>,
    /// Kept so a round still running after the run can finish in place.
    _dir: tempfile::TempDir,
}

/// Run contracts `a` (writes `a.flag`) and `b` (writes `b.flag`) at once.
/// Each worker writes its file, then idles until `release` says both have
/// been in flight long enough, then finishes.
async fn run_pair(
    validation: &str,
    delay: Duration,
    release: impl Fn(&Probe, &RunStore) -> bool + Send + 'static,
) -> Outcome {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README"), "hello\n").expect("write");
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).expect("mkdir");
    let manager = Arc::new(WorkspaceManager::open(&root, &dir.path().join("ws")).expect("manager"));
    let branch = ensure_integration_branch(&root, "r1").expect("branch");
    let merge_back = Arc::new(
        MergeBack::open(
            manager.clone(),
            &branch,
            kgraf_sandbox::Identity {
                name: "Test Owner".into(),
                email: "owner@example.invalid".into(),
            },
        )
        .expect("merge-back"),
    );
    let workspaces = Arc::new(GitWorkspaces::new(manager.clone(), merge_back.clone()));
    let graph = Arc::new(Mutex::new(
        GraphStore::open(&state.join("graph.db"), &root).expect("graph"),
    ));
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));

    let released = Arc::new(AtomicBool::new(false));
    let hold = released.clone();
    let worker = Arc::new(Script {
        reply: Box::new(move |text, round| {
            let file = if text.contains("WRITE a.flag") {
                "a.flag"
            } else {
                "b.flag"
            };
            if round == 0 {
                return vec![call(
                    "w",
                    "write_file",
                    json!({"path": file, "content": file}),
                )];
            }
            if !hold.load(Ordering::SeqCst) && round < 55 {
                return vec![call(&format!("nap{round}"), "nap", json!({"r": round}))];
            }
            vec![Content::text("done")]
        }),
        seen: Mutex::new(Vec::new()),
    });
    let contract = |id: &str| {
        json!({
            "id": id,
            "instruction": format!("make {id} WRITE {id}.flag"),
            "acceptance": format!("{id}.flag exists"),
            "writes": [{"path": format!("{id}.flag")}],
            "depends_on": [],
            "tools": ["write_file"],
            "attempts": 1,
        })
    };
    let plan = json!({"contracts": [contract("a"), contract("b")]}).to_string();
    let planner = Arc::new(Script {
        reply: Box::new(move |_, _| vec![Content::text(plan.clone())]),
        seen: Mutex::new(Vec::new()),
    });
    let grounder = Arc::new(Script {
        reply: Box::new(|_, _| vec![Content::text(r#"{"serves":true,"reason":"on track"}"#)]),
        seen: Mutex::new(Vec::new()),
    });

    let line = SessionLine::new();
    let mut registry = ToolRegistry::new();
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(WriteFile), Arc::new(Nap)];
    tools.extend(session_tools(&line));
    for tool in tools {
        registry
            .register_builtin(tool, Exposure::Prefix)
            .expect("register");
    }
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "vendor-e2e".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a kGraf worker.")],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("prefix");
    let mut env = WorkerEnv::new(ledger.clone(), worker.clone(), Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    let mut prices = PriceTable::new();
    prices.set(
        MODEL,
        ModelPrice {
            uncached_input: 1_000_000,
            cache_read: 100_000,
            cache_write: 1_250_000,
            output: 2_000_000,
            reasoning: 2_000_000,
        },
    );
    let caller = |p: Arc<Script>| ModelCaller {
        provider: p,
        ledger: ledger.clone(),
        model: MODEL.into(),
        max_output_tokens: 2_000,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    };
    let services = Services {
        graph: graph.clone(),
        ledger: ledger.clone(),
        prices,
        routes: vec![Route {
            name: "alpha".into(),
            vendor: "vendor-e2e".into(),
            env: Arc::new(env),
            context: ContextConfig::for_model(200_000).expect("context"),
            readiness: Arc::new(AlwaysReady),
            local: false,
        }],
        decomposer: Arc::new(ModelDecomposer(caller(planner))),
        grounder: Arc::new(ModelGrounder(caller(grounder))),
        verifier: Arc::new(CommandVerifier {
            command: "true".into(),
            repo: root.clone(),
            workspaces: workspaces.clone(),
            merge_back: merge_back.clone(),
            read_denies: Vec::new(),
            timeout: Duration::from_secs(120),
            permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
            root_gate: false,
        }),
        workspaces: workspaces.clone(),
        clock: Arc::new(SystemClock),
        admission: Admission::new(Limits::default()),
        runs_dir: state.join("runs"),
        profiles: Arc::new(std::sync::Mutex::new(
            kgraf_core::model_profile::ProfileStore::default(),
        )),
        profiles_dir: state.join("profiles"),
    };
    let probe = Arc::new(Probe {
        inner: CommandGateRunner {
            commands: vec![validation.to_owned()],
            manager,
            merge_back,
            read_denies: Vec::new(),
            timeout: Duration::from_secs(60),
            output_cap: 2_000,
        },
        delay,
        started_ms: Mutex::new(None),
        finished_ms: Mutex::new(None),
    });
    let config = OrchestratorConfig {
        tick: Duration::from_millis(10),
        step_budget: 60,
        speculate_every: Some(Duration::from_millis(20)),
        ..OrchestratorConfig::default()
    };
    let o = Orchestrator::new(config, services)
        .expect("orchestrator")
        .with_gate_runner(probe.clone());
    line.bind(o.handle()).expect("bind");
    let watch = {
        let probe = probe.clone();
        let store = RunStore::new(graph.clone());
        tokio::spawn(async move {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !release(&probe, &store) && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            released.store(true, Ordering::SeqCst);
        })
    };
    let report = tokio::time::timeout(
        Duration::from_secs(120),
        o.start(RunId::new("r1"), "make a and b".into()),
    )
    .await
    .expect("the run ended in time")
    .expect("run");
    watch.await.expect("watch");
    let trace = TraceLog::open(&state.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    assert_eq!(report.status, RunStatus::Completed, "{trace:#?}");
    let record = RunStore::new(graph)
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    Outcome {
        trace,
        record,
        branch_log: git(&root, &["log", "--format=%s", &branch]),
        probe,
        _dir: dir,
    }
}

fn integrations(trace: &[TraceEvent]) -> Vec<(usize, i64, String)> {
    trace
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            TraceEvent::Integration {
                at_ms,
                contract,
                outcome,
                ..
            } if outcome == "merged" => Some((i, *at_ms, contract.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_semantic_conflict_between_two_in_flight_workers_is_reported_before_either_lands() {
    // Each alone passes; together they fail. Workers hold until the record
    // carries the finding, so it can only have come from in-flight trees.
    let out = run_pair(
        "test ! -f a.flag || test ! -f b.flag",
        Duration::ZERO,
        |_, store| {
            store
                .load(&RunId::new("r1"))
                .ok()
                .flatten()
                .is_some_and(|r| !r.coordination.speculative_conflicts.is_empty())
        },
    )
    .await;
    let found = &out.record.coordination.speculative_conflicts;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].contracts, ["a".to_owned(), "b".to_owned()]);
    assert_eq!(found[0].kind, ConflictKind::Semantic);
    // The pair and each member alone were validated, and the run says so.
    assert!(out.record.coordination.speculation.validations >= 3);
    assert!(out.record.coordination.speculation.rounds >= 1);
    let reported = out
        .trace
        .iter()
        .position(|e| matches!(e, TraceEvent::Speculation { outcome, .. } if outcome == "semantic"))
        .expect("the conflict is traced");
    let landed = integrations(&out.trace);
    assert_eq!(
        landed.len(),
        2,
        "both still land: a gate, not speculation, blocks"
    );
    assert!(
        landed.iter().all(|(i, _, _)| reported < *i),
        "reported before either landed"
    );
    assert!(
        !out.branch_log.contains("speculative"),
        "no speculative commit on the integration branch:\n{}",
        out.branch_log
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_pair_lands_while_its_speculative_validation_is_still_running() {
    // Validation passes, and is held 4 s. Workers finish as soon as it has
    // started, so both are in flight when the round begins.
    let out = run_pair("test -f README", Duration::from_secs(4), |probe, _| {
        probe.started_ms.lock().expect("lock").is_some()
    })
    .await;
    assert!(out.record.coordination.speculative_conflicts.is_empty());
    let landed = integrations(&out.trace);
    assert_eq!(landed.len(), 2, "{landed:?}");
    // The run ended; the round it started finishes on its own.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let finished = loop {
        if let Some(at) = *out.probe.finished_ms.lock().expect("lock") {
            break at;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the round never ended"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        landed.iter().all(|(_, at, _)| *at < finished),
        "merge-back never waited for speculation: {landed:?}, validation done at {finished}"
    );
    assert!(
        !out.branch_log.contains("speculative"),
        "{}",
        out.branch_log
    );
}
