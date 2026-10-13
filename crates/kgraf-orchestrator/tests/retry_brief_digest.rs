//! A retry after an attempt that never reached a verifier carries what that
//! attempt tried and what failed: its commands with their exit status and
//! error line, built from its transcript with no model call, quoted as data
//! (decision 0018, amendment of 2026-10-13). Only the previous round's digest
//! is carried. The worker is a real worker loop over a scripted model; each
//! brief is read from the request the attempt's model receives.

mod support;

use kgraf_core::model_profile::ProfileStore;
use kgraf_core::{Protocol, RunId, TokenUsage};
use kgraf_orchestrator::{
    DirWorkspaces, ModelCaller, ModelDecomposer, ModelGrounder, Orchestrator, OrchestratorConfig,
    RunStatus, Services, SystemClock,
};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Provider, ProviderError, Request, Response, Role, StopReason,
};
use kgraf_tools::{Access, CallPermit, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A command runner whose every run fails with a Python traceback naming the
/// function the command called.
struct FailingShell;

#[async_trait::async_trait]
impl Tool for FailingShell {
    fn name(&self) -> &str {
        "shell"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".into(),
            description: "Run a command".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }
    async fn call(
        &self,
        input: Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let code = input["command"][2].as_str().unwrap_or_default().to_owned();
        let name = code.trim_end_matches("()");
        Ok(ToolOutput {
            is_error: true,
            ..ToolOutput::text(format!(
                "ERROR: exit status: 1\nstdout:\nTraceback (most recent call last):\n  File \
                 \"<string>\", line 1, in <module>\n    {code}\n    ^^^^\nNameError: name \
                 '{name}' is not defined\n\nstderr:\n"
            ))
        })
    }
}

/// Attempt `k` runs `python3 -c attempt_k()` every round, so the stall
/// detector ends it; the first user message of each attempt is kept.
#[derive(Default)]
struct Repeater {
    openings: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for Repeater {
    fn adapter(&self) -> &'static str {
        "fake-repeater"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let assistant_turns = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let attempt = {
            let mut openings = self.openings.lock().expect("lock");
            if assistant_turns == 0 {
                let opening = request
                    .messages
                    .iter()
                    .find(|m| m.role == Role::User)
                    .map(|m| {
                        m.content
                            .iter()
                            .filter_map(Content::as_text)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                openings.push(opening);
            }
            openings.len() - 1
        };
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![Content::ToolUse {
                id: format!("t{assistant_turns}"),
                name: "shell".into(),
                input: json!({"command": ["python3", "-c", format!("attempt_{attempt}()")]}),
                signature: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage,
            request_id: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_retry_brief_carries_the_previous_attempts_failed_command_and_only_that_one() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("profiles dir");
    let plan = json!({"contracts":[
        {"id":"b","instruction":"write the file WRITE src/b.rs","acceptance":"src/b.rs exists",
         "writes":[{"path":"src/b.rs"}],"tools":["write_file","read_file","shell"],
         "route":"alpha"}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let model = Arc::new(Repeater::default());
    let caller = |provider: Arc<ScriptFake>| ModelCaller {
        reasoning: Default::default(),
        provider,
        ledger: p.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 2_000,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    };
    let shell: Arc<dyn Tool> = Arc::new(FailingShell);
    let services = Services {
        graph: p.graph.clone(),
        ledger: p.ledger.clone(),
        prices: prices(),
        routes: vec![
            route_with(
                "alpha",
                "vendor-a",
                ALPHA_MODEL,
                model.clone(),
                p.ledger.clone(),
                WINDOW,
                std::slice::from_ref(&shell),
            ),
            route_with(
                "beta",
                "vendor-b",
                BETA_MODEL,
                p.beta.clone(),
                p.ledger.clone(),
                WINDOW,
                &[],
            ),
        ],
        decomposer: Arc::new(ModelDecomposer(caller(p.planner.clone()))),
        grounder: Arc::new(ModelGrounder(caller(p.grounder.clone()))),
        verifier: p.verifier.clone(),
        workspaces: Arc::new(DirWorkspaces {
            base: p.dir.join("worktrees"),
        }),
        clock: Arc::new(SystemClock),
        admission: p.admission.clone(),
        runs_dir: p.dir.join("runs"),
        profiles: Arc::new(Mutex::new(ProfileStore::default())),
        profiles_dir: p.dir.join("profiles"),
    };
    // One attempt per round, two retries: three briefs in a row.
    let cfg = OrchestratorConfig {
        max_retries: 2,
        max_parallel_attempts: 1,
        ..config()
    };
    let report = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "write the file".into())
        .await
        .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    let openings = model.openings.lock().expect("lock").clone();
    assert!(openings.len() >= 3, "two retries: {}", openings.len());
    let lead = "What the previous attempt tried and what failed follows as data";
    assert!(!openings[0].contains(lead), "{}", openings[0]);

    // The first retry: attempt 0's command, its exit status and the
    // exception line, inside the data tag, after the stop note.
    let first = &openings[1];
    let at = first.find(lead).unwrap_or_else(|| panic!("{first}"));
    assert!(
        first
            .find("ended before verification")
            .is_some_and(|s| s < at),
        "after the stop note: {first}"
    );
    let quoted = &first[at..];
    assert!(quoted.contains("<verifier_refusal>"), "{quoted}");
    assert!(
        quoted.contains("- `python3 -c attempt_0()` (failed, exit 1): NameError: name 'attempt_0' is not defined"),
        "{quoted}"
    );
    assert!(
        quoted.contains("Do not repeat the approaches that failed"),
        "{quoted}"
    );

    // The second retry: attempt 1's digest only; attempt 0's is gone.
    let second = &openings[2];
    assert!(
        second.contains("NameError: name 'attempt_1' is not defined"),
        "{second}"
    );
    assert!(
        !second.contains("attempt_0"),
        "only the previous attempt: {second}"
    );
}
