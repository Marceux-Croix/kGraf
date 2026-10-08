//! The compaction point a user sets decides when a worker compacts or stops.
//!
//! These tests move the point the way a user does, through the desktop
//! slider's shared limits and through `--point`, build the thresholds with the
//! host's own `RunConfig::context`, and run a worker against a scripted
//! provider whose reported context grows every round. They assert on the
//! context size at which the runtime compacted or handed back, not on a stored
//! value. A setting that was saved and never read would fail them: both points
//! below would compact at the default 80%.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_host::RunConfig;
use kgraf_host::limits::UserLimits;
use kgraf_provider::capability::Capabilities;
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, Role, StopReason,
};
use kgraf_runtime::{
    CallMode, ContextConfig, NeutralPrefixEncoder, PrefixRegistry, PrefixSnapshot, SUMMARY_REQUEST,
    SharedPrefix, SpawnOptions, StopPointPolicy, WorkerBrief, WorkerEnv, WorkerEvent,
    WorkerOutcome, WorkerResult, spawn_worker,
};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const MODEL: &str = "fake-model";
/// The effective window the run file below gives every worker.
const WINDOW: u64 = 100_000;
/// Reported input tokens added by each round.
const STEP: u64 = 2_000;

/// Reports a context that grows by [`STEP`] tokens every round, so the round
/// that crosses a point is known to within one step.
struct Growing;

#[async_trait::async_trait]
impl Provider for Growing {
    fn adapter(&self) -> &'static str {
        "fake"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        let summary = request.messages.last().is_some_and(|m| {
            m.content
                .iter()
                .any(|c| c.as_text() == Some(SUMMARY_REQUEST))
        });
        let round = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count() as u64
            + 1;
        let compacted = request.messages[0].content.iter().any(|c| {
            c.as_text()
                .is_some_and(|t| t.contains(kgraf_runtime::COMPACTION_NOTICE_HEADER))
        });
        let (text, calls, stop, input) = if summary {
            (
                "{\"goal\": \"g\", \"decisions\": [], \"files_touched\": [], \
                 \"open_items\": [\"more\"], \"constraints\": []}"
                    .to_owned(),
                Vec::new(),
                StopReason::EndTurn,
                100,
            )
        } else if compacted {
            // Small after a compaction: the worker finishes.
            ("done".to_owned(), Vec::new(), StopReason::EndTurn, STEP)
        } else {
            (
                "working".to_owned(),
                vec![Content::ToolUse {
                    id: format!("t{round}"),
                    name: "write_file".into(),
                    input: json!({"path": format!("f{round}.txt")}),
                    signature: None,
                }],
                StopReason::ToolUse,
                STEP * round,
            )
        };
        let usage = TokenUsage {
            uncached_input: input,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        let mut content = vec![Content::text(text)];
        content.extend(calls);
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content,
            stop_reason: stop,
            usage,
            request_id: None,
        })
    }

    fn capabilities(&self) -> Capabilities<'_> {
        Capabilities::none()
    }
}

/// Writes a file in the worktree, so each round is measured progress and a
/// worker at its stop point hands back instead of compacting for want of any.
struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: "Write a file".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        Access::none()
            .write_path(input["path"].as_str().unwrap_or_default())
            .map_err(|source| ToolError::Access {
                tool: "write_file".into(),
                source,
            })
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let path = input["path"].as_str().unwrap_or_default();
        std::fs::write(ctx.workspace_root.join(path), "x").map_err(|e| ToolError::Internal {
            tool: "write_file".into(),
            reason: e.to_string(),
        })?;
        Ok(ToolOutput::text(format!("wrote {path}")))
    }
}

fn env() -> Arc<WorkerEnv> {
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(WriteFile), Exposure::Prefix)
        .expect("register");
    let prefix = PrefixSnapshot::new(
        SharedPrefix {
            vendor: "fake-vendor".into(),
            model: MODEL.into(),
            system: vec![Content::text("You are a kGraf worker.")],
            tools: registry.prefix_definitions(),
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &NeutralPrefixEncoder,
    )
    .expect("snapshot");
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let mut env = WorkerEnv::new(ledger, Arc::new(Growing), Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    Arc::new(env)
}

/// The run file every case loads; it sets no point, so the point comes from
/// the user's limits, as the desktop sends it.
fn run_file(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("run.json");
    std::fs::write(
        &path,
        format!(r#"{{"routes": ["kimi:model"], "prices": "p.json", "window": {WINDOW}}}"#),
    )
    .expect("write run file");
    path
}

/// Run one worker with the thresholds `context` and the stop policy.
async fn run(
    dir: &Path,
    context: ContextConfig,
    stop_point: StopPointPolicy,
) -> (Vec<WorkerEvent>, WorkerResult) {
    let brief = WorkerBrief {
        task: "task-w1".into(),
        worker: "w1".into(),
        instruction: "Do the work.".into(),
        workspace_root: dir.to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: 200,
        context,
        write_scope: None,
    };
    let options = SpawnOptions {
        stop_point,
        ..SpawnOptions::default()
    };
    let mut handle = spawn_worker(env(), brief, options).await.expect("spawn");
    let mut events = Vec::new();
    let finish = async {
        while let Some(event) = handle.next_event().await {
            events.push(event);
        }
        handle.result().await.expect("result")
    };
    let result = tokio::time::timeout(Duration::from_secs(30), finish)
        .await
        .expect("in time");
    (events, result)
}

/// The context size at which the worker compacted: it must be the first round
/// at or past the point, within one growth step of it.
fn assert_compacted_at(events: &[WorkerEvent], result: &WorkerResult, percent: u64) {
    let point = WINDOW * percent / 100;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    let notices: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Compacted { notice, .. } => Some(notice),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "exactly one automatic compaction");
    let notice = notices[0];
    assert_eq!(notice.stop_tokens, point, "the notice states the point");
    assert!(
        notice.at_tokens >= point,
        "compacted at {} tokens, before the {percent}% point of {point}",
        notice.at_tokens
    );
    assert!(
        notice.at_tokens < point + 2 * STEP,
        "compacted at {} tokens, late for the {percent}% point of {point}",
        notice.at_tokens
    );
}

#[tokio::test]
async fn automatic_compaction_fires_at_the_point_the_slider_sets() {
    let mut fired = Vec::new();
    for percent in [30u8, 60] {
        let dir = tempfile::tempdir().expect("dir");
        // What the desktop sends after the slider moves.
        let limits = UserLimits {
            compaction_point: Some(percent),
            ..UserLimits::default()
        };
        let config =
            RunConfig::load_with(dir.path(), &run_file(dir.path()), &limits).expect("load");
        let context = config.context().expect("thresholds");
        let (events, result) = run(dir.path(), context, StopPointPolicy::Compact).await;
        assert_compacted_at(&events, &result, u64::from(percent));
        fired.push(percent);
    }
    assert_eq!(fired, [30, 60], "two different points were exercised");
}

#[tokio::test]
async fn automatic_compaction_fires_at_the_point_the_command_line_sets() {
    for percent in [25u8, 70] {
        let dir = tempfile::tempdir().expect("dir");
        // `--point` sets the run configuration's field directly.
        let mut config = RunConfig::new(dir.path(), vec!["kimi:model".into()], "p.json");
        config.window = WINDOW;
        config.point = Some(percent);
        let context = config.context().expect("thresholds");
        let (events, result) = run(dir.path(), context, StopPointPolicy::Compact).await;
        assert_compacted_at(&events, &result, u64::from(percent));
    }
}

#[tokio::test]
async fn with_no_point_set_the_worker_compacts_at_the_default() {
    let dir = tempfile::tempdir().expect("dir");
    let config = RunConfig::load_with(dir.path(), &run_file(dir.path()), &UserLimits::default())
        .expect("load");
    let (events, result) = run(
        dir.path(),
        config.context().expect("thresholds"),
        StopPointPolicy::Compact,
    )
    .await;
    assert_compacted_at(&events, &result, 80);
}

#[tokio::test]
async fn a_worker_that_hands_back_stops_at_the_point_the_user_sets() {
    for percent in [30u8, 60] {
        let dir = tempfile::tempdir().expect("dir");
        let limits = UserLimits {
            compaction_point: Some(percent),
            ..UserLimits::default()
        };
        let config =
            RunConfig::load_with(dir.path(), &run_file(dir.path()), &limits).expect("load");
        let (events, result) = run(
            dir.path(),
            config.context().expect("thresholds"),
            StopPointPolicy::HandBack,
        )
        .await;
        let point = WINDOW * u64::from(percent) / 100;
        let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
            panic!("expected a hand-back at {percent}%: {:?}", result.outcome);
        };
        assert!(handoff.context_tokens >= point, "{handoff:?}");
        assert!(handoff.context_tokens < point + 2 * STEP, "{handoff:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Compacted { .. })),
            "a hand-back does not compact"
        );
    }
}

#[test]
fn a_point_outside_the_range_is_refused_not_clamped() {
    let dir = tempfile::tempdir().expect("dir");
    for bad in [4u8, 96] {
        let limits = UserLimits {
            compaction_point: Some(bad),
            ..UserLimits::default()
        };
        let config =
            RunConfig::load_with(dir.path(), &run_file(dir.path()), &limits).expect("load");
        assert!(config.context().is_err(), "{bad}% must be refused");
    }
}
