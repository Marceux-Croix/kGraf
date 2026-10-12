//! A model that pastes a complete patch block into its reply and ends the turn
//! with no tool call. Tested on the mechanism: a scripted provider plays the
//! model, and a stand-in `apply_patch` records what it was called with.
//!
//! 1. One complete block in a turn with no tool call is run as an
//!    `apply_patch` call: it is recorded like a call the model made, its
//!    result goes back to the model, and a notice says what happened.
//! 2. Two blocks in one reply are not run.
//! 3. A block beside a real tool call in the same reply is not run; only the
//!    model's own call is.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, StopReason,
};
use kgraf_runtime::{
    CallMode, ContextConfig, NeutralPrefixEncoder, NoticeKind, PrefixRegistry, PrefixSnapshot,
    SharedPrefix, SpawnOptions, ToolChoiceSupport, WorkerBrief, WorkerEnv, WorkerEvent,
    WorkerHandle, WorkerOutcome, WorkerResult, spawn_worker,
};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A complete patch block.
const BLOCK: &str = "*** Begin Patch\n*** Add File: a.rs\n+pub fn a() {}\n*** End Patch";

/// A second, different complete block.
const OTHER_BLOCK: &str = "*** Begin Patch\n*** Add File: b.rs\n+pub fn b() {}\n*** End Patch";

// ---------------------------------------------------------------- scripted provider

/// One reply: text, and the tool calls it carries.
struct Reply {
    text: String,
    calls: Vec<(String, String, Value)>,
}

fn says(text: &str) -> Reply {
    Reply {
        text: text.into(),
        calls: Vec::new(),
    }
}

type Script = dyn Fn(&Request) -> Reply + Send + Sync;

struct Fake {
    script: Box<Script>,
}

impl Fake {
    fn new(script: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
        })
    }
}

#[async_trait::async_trait]
impl Provider for Fake {
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
        let reply = (self.script)(request);
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        let stop = if reply.calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        };
        let mut content = vec![Content::text(reply.text)];
        for (id, name, input) in reply.calls {
            content.push(Content::ToolUse {
                id,
                name,
                input,
                signature: None,
            });
        }
        Ok(Response {
            finish_reason: Some("stop".into()),
            model: request.model.clone(),
            content,
            stop_reason: stop,
            usage,
            request_id: None,
        })
    }
}

/// How many tool results the request carries.
fn results_in(request: &Request) -> usize {
    request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|c| matches!(c, Content::ToolResult { .. }))
        .count()
}

// ---------------------------------------------------------------- tools

/// Stands in for the patch tool: records the `patch` it was given and writes
/// `a.rs`.
struct ApplyPatch {
    patches: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".into(),
            description: "Apply a patch".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, _input: &Value) -> Result<Access, ToolError> {
        Access::none()
            .write_path("a.rs")
            .map_err(|source| ToolError::Access {
                tool: "apply_patch".into(),
                source,
            })
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        self.patches
            .lock()
            .expect("lock")
            .push(input["patch"].as_str().unwrap_or_default().to_owned());
        std::fs::write(ctx.workspace_root.join("a.rs"), "x").map_err(|e| ToolError::Internal {
            tool: "apply_patch".into(),
            reason: e.to_string(),
        })?;
        Ok(ToolOutput::text("applied a.rs"))
    }
}

// ---------------------------------------------------------------- harness

const MODEL: &str = "fake-model";

fn env(provider: Arc<Fake>, patches: Arc<Mutex<Vec<String>>>) -> Arc<WorkerEnv> {
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(ApplyPatch { patches }), Exposure::Prefix)
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
    let mut env = WorkerEnv::new(ledger, provider, Arc::new(registry), prefix);
    env.prefixes = Arc::new(PrefixRegistry::new());
    env.settings.call_mode = CallMode::Complete;
    env.settings.tool_choice = ToolChoiceSupport::default();
    Arc::new(env)
}

fn brief(root: &Path) -> WorkerBrief {
    WorkerBrief {
        task: "task-w1".into(),
        worker: "w1".into(),
        instruction: "Create a.rs.".into(),
        workspace_root: root.to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: 20,
        context: ContextConfig::for_model(100_000).expect("config"),
        write_scope: Some(vec![PathBuf::from("a.rs")]),
    }
}

/// The events, the result and the patches the tool saw.
async fn run(provider: Arc<Fake>) -> (Vec<WorkerEvent>, WorkerResult, Vec<String>) {
    run_seeded(provider, &[]).await
}

/// [`run`] on a worktree that already holds the `(path, content)` files.
async fn run_seeded(
    provider: Arc<Fake>,
    files: &[(&str, &str)],
) -> (Vec<WorkerEvent>, WorkerResult, Vec<String>) {
    let dir = tempfile::tempdir().expect("dir");
    for (path, content) in files {
        std::fs::write(dir.path().join(path), content).expect("seed");
    }
    let patches = Arc::new(Mutex::new(Vec::new()));
    let handle = spawn_worker(
        env(provider, patches.clone()),
        brief(dir.path()),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    let seen = patches.lock().expect("lock").clone();
    (events, result, seen)
}

async fn drain(mut handle: WorkerHandle) -> (Vec<WorkerEvent>, WorkerResult) {
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = handle.next_event().await {
            events.push(event);
        }
        handle.result().await.expect("result")
    };
    let result = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("in time");
    (events, result)
}

fn notices_of(events: &[WorkerEvent], wanted: NoticeKind) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Notice { kind, text } if *kind == wanted => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The tool calls the loop started, as (tool, call id).
fn started(events: &[WorkerEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::ToolCallStarted { tool, call_id, .. } => {
                Some((tool.clone(), call_id.clone()))
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------- tests

/// One complete block, text around it, no tool call: the loop runs it as an
/// `apply_patch` call. The tool gets the block alone, the call is in the event
/// stream like any call, the result goes back to the model, the notice says
/// what happened, and no no-change notice is spent.
#[tokio::test]
async fn one_pasted_block_is_applied_as_an_apply_patch_call() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => says(&format!("Here is the change:\n```\n{BLOCK}\n```")),
        _ => says("Done."),
    });
    let (events, result, patches) = run(provider.clone()).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(patches, vec![BLOCK.to_owned()]);
    let calls = started(&events);
    assert_eq!(
        calls,
        vec![("apply_patch".to_owned(), "kgraf-pasted-patch-1".to_owned())]
    );
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::ToolCallFinished { call_id, is_error: false, .. }
            if call_id == "kgraf-pasted-patch-1"
    )));
    let notices = notices_of(&events, NoticeKind::PatchAsText);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("held a patch as text; kGraf applied it as an apply_patch call"),
        "{}",
        notices[0]
    );
    assert!(notices_of(&events, NoticeKind::NoChange).is_empty());
    assert!(
        result.changed_paths.iter().any(|p| p.ends_with("a.rs")),
        "{result:?}"
    );
}

/// Two blocks in one reply: which one the model meant is unknown, so neither
/// runs, and the turn is judged as it was before (a no-change end).
#[tokio::test]
async fn two_pasted_blocks_are_not_applied() {
    let provider = Fake::new(|_| says(&format!("First:\n{BLOCK}\nor:\n{OTHER_BLOCK}")));
    let (events, _result, patches) = run(provider).await;
    assert!(patches.is_empty(), "{patches:?}");
    assert!(started(&events).is_empty());
    assert!(notices_of(&events, NoticeKind::PatchAsText).is_empty());
    assert!(!notices_of(&events, NoticeKind::NoChange).is_empty());
}

/// A block beside a real tool call in the same reply: only the model's own
/// call runs, and no notice says kGraf applied anything.
#[tokio::test]
async fn a_block_beside_a_tool_call_is_not_applied() {
    let provider = Fake::new(|request| match results_in(request) {
        0 => Reply {
            text: format!("Applying:\n{OTHER_BLOCK}"),
            calls: vec![("t1".into(), "apply_patch".into(), json!({"patch": BLOCK}))],
        },
        _ => says("Done."),
    });
    let (events, result, patches) = run(provider).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(patches, vec![BLOCK.to_owned()], "only the model's own call");
    assert_eq!(
        started(&events),
        vec![("apply_patch".to_owned(), "t1".to_owned())]
    );
    assert!(notices_of(&events, NoticeKind::PatchAsText).is_empty());
}

/// A block whose Update no longer matches the file (a final answer quoting a
/// patch that was already applied): not adopted, so the turn is judged as it
/// was before and the failed call is not made.
#[tokio::test]
async fn a_block_whose_update_no_longer_matches_is_not_adopted() {
    let stale = "*** Begin Patch\n*** Update File: a.rs\n@@\n-pub fn old() {}\n+pub fn new() {}\n*** End Patch";
    let provider = Fake::new(move |_| says(&format!("I applied this:\n{stale}")));
    let (events, _result, patches) = run_seeded(provider, &[("a.rs", "pub fn new() {}\n")]).await;
    assert!(patches.is_empty(), "{patches:?}");
    assert!(started(&events).is_empty());
    assert!(notices_of(&events, NoticeKind::PatchAsText).is_empty());
    assert!(!notices_of(&events, NoticeKind::NoChange).is_empty());
}

/// Counter-case: the same Update against a file it does match is adopted.
#[tokio::test]
async fn a_block_whose_update_matches_is_adopted() {
    let fresh = "*** Begin Patch\n*** Update File: a.rs\n@@\n-pub fn old() {}\n+pub fn new() {}\n*** End Patch";
    let provider = Fake::new(move |request| match results_in(request) {
        0 => says(&format!("Change:\n{fresh}")),
        _ => says("Done."),
    });
    let (events, _result, patches) = run_seeded(provider, &[("a.rs", "pub fn old() {}\n")]).await;
    assert_eq!(patches.len(), 1, "{patches:?}");
    assert_eq!(notices_of(&events, NoticeKind::PatchAsText).len(), 1);
}

/// An unfinished block (no end line) is no block: nothing runs.
#[tokio::test]
async fn an_unfinished_block_is_not_applied() {
    let provider = Fake::new(|_| says("*** Begin Patch\n*** Add File: a.rs\n+pub fn a() {}"));
    let (events, _result, patches) = run(provider).await;
    assert!(patches.is_empty(), "{patches:?}");
    assert!(notices_of(&events, NoticeKind::PatchAsText).is_empty());
}
