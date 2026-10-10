//! A model that says what it will do and ends its turn with no tool call, on
//! a contract that writes files. Tested on the mechanism: a scripted provider
//! records every request and answers each one the way such a model does, and
//! each test asserts on those requests, the event stream and the result.
//!
//! 1. The notice quotes what the model said and names the tool that does it;
//!    a model that acts on that notice completes the attempt.
//! 2. The next request requires a tool call where the adapter has a wire form
//!    and the route is not recorded as ignoring it; the decision is an event
//!    either way, with its reason.
//! 3. The notices are bounded per attempt; past the bound the attempt fails
//!    with a reason that names the count and the contract's writes.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, Role, StopReason,
    ToolChoice,
};
use kgraf_runtime::{
    CallMode, ContextConfig, MAX_NO_CHANGE_NOTICES, NeutralPrefixEncoder, NoticeKind,
    PrefixRegistry, PrefixSnapshot, SharedPrefix, SpawnOptions, ToolChoiceSupport, WorkerBrief,
    WorkerEnv, WorkerEvent, WorkerHandle, WorkerOutcome, WorkerResult, spawn_worker,
};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the model of the recorded runs said before it ended its turn.
const ANNOUNCEMENT: &str = "I'll create all three files in one patch.";

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

fn patch(path: &str) -> Reply {
    Reply {
        text: String::new(),
        calls: vec![(
            format!("p-{path}"),
            "apply_patch".into(),
            json!({"path": path, "content": "x"}),
        )],
    }
}

type Script = dyn Fn(&Request) -> Reply + Send + Sync;

struct Fake {
    script: Box<Script>,
    /// What `forces_tool_choice` answers: whether the adapter has a wire form.
    wire_form: bool,
    requests: Mutex<Vec<Request>>,
}

impl Fake {
    fn new(
        wire_form: bool,
        script: impl Fn(&Request) -> Reply + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            wire_form,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
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

    fn forces_tool_choice(&self) -> bool {
        self.wire_form
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.requests.lock().expect("lock").push(request.clone());
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

/// The text of the last user turn of a request: where the loop's notice is.
fn last_user_text(request: &Request) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|c| c.as_text())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Whether the request's last user turn is a notice that quotes the
/// announcement and names `apply_patch`: the notice a model needs to act.
fn told_specifically(request: &Request) -> bool {
    let text = last_user_text(request);
    text.contains(&format!("\"{ANNOUNCEMENT}\"")) && text.contains("Call apply_patch now")
}

/// Whether a tool result is in the request: the worker already wrote.
fn wrote(request: &Request) -> bool {
    request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|c| matches!(c, Content::ToolResult { .. }))
}

// ---------------------------------------------------------------- tools

/// Writes the file `path` in the worktree, standing in for the real patch tool.
struct ApplyPatch;

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".into(),
            description: "Write a file".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        Access::none()
            .write_path(input["path"].as_str().unwrap_or_default())
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
        let path = input["path"].as_str().unwrap_or_default();
        std::fs::write(ctx.workspace_root.join(path), "x").map_err(|e| ToolError::Internal {
            tool: "apply_patch".into(),
            reason: e.to_string(),
        })?;
        Ok(ToolOutput::text(format!("wrote {path}")))
    }
}

// ---------------------------------------------------------------- harness

const MODEL: &str = "fake-model";

fn env(provider: Arc<Fake>, support: ToolChoiceSupport) -> Arc<WorkerEnv> {
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(ApplyPatch), Exposure::Prefix)
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
    env.settings.tool_choice = support;
    Arc::new(env)
}

fn brief(root: &Path) -> WorkerBrief {
    WorkerBrief {
        task: "task-w1".into(),
        worker: "w1".into(),
        instruction: "Create the Cargo workspace and the wcl-core crate.".into(),
        workspace_root: root.to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: 20,
        context: ContextConfig::for_model(100_000).expect("config"),
        write_scope: Some(vec![
            PathBuf::from("Cargo.toml"),
            PathBuf::from("wcl-core/Cargo.toml"),
        ]),
    }
}

async fn run(provider: Arc<Fake>, support: ToolChoiceSupport) -> (Vec<WorkerEvent>, WorkerResult) {
    let dir = tempfile::tempdir().expect("dir");
    let handle = spawn_worker(
        env(provider, support),
        brief(dir.path()),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    drain(handle).await
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

fn no_change_notices(events: &[WorkerEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Notice {
                kind: NoticeKind::NoChange,
                text,
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn tool_choices(events: &[WorkerEvent]) -> Vec<(bool, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::ToolChoice { forced, detail } => Some((*forced, detail.clone())),
            _ => None,
        })
        .collect()
}

/// The recorded pattern: the model announces, ends its turn, and acts only
/// on a notice that quotes it and names `apply_patch`. The notice it gets
/// says both; the next request requires a tool call (the route is not
/// measured, so the event says UNVERIFIED); the model writes and completes.
#[tokio::test]
async fn an_announcement_gets_a_notice_that_quotes_it_and_names_the_tool_and_then_a_forced_call() {
    let provider = Fake::new(true, |request| {
        if wrote(request) {
            says("Created Cargo.toml.")
        } else if told_specifically(request) {
            patch("Cargo.toml")
        } else {
            says(ANNOUNCEMENT)
        }
    });
    let (events, result) = run(provider.clone(), ToolChoiceSupport::Unmeasured).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    let notices = no_change_notices(&events);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with(
            "You ended without making any change; this contract writes Cargo.toml, \
             wcl-core/Cargo.toml."
        ),
        "{}",
        notices[0]
    );
    assert!(
        !notices[0].contains("last notice"),
        "the first notice is not the last: {}",
        notices[0]
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "announce, act, answer");
    assert_eq!(requests[0].tool_choice, ToolChoice::Auto);
    assert_eq!(
        requests[1].tool_choice,
        ToolChoice::Required,
        "the round after the notice requires a tool call"
    );
    assert_eq!(requests[2].tool_choice, ToolChoice::Auto, "only that round");
    let choices = tool_choices(&events);
    assert_eq!(choices.len(), 1, "{choices:?}");
    assert!(choices[0].0);
    assert!(choices[0].1.contains("UNVERIFIED"), "{}", choices[0].1);
    // The event comes before the model call of the round it is for.
    let at = |pred: &dyn Fn(&WorkerEvent) -> bool| events.iter().position(pred).expect("event");
    let choice_at = at(&|e| matches!(e, WorkerEvent::ToolChoice { .. }));
    let second_call_at = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, WorkerEvent::ModelCall { .. }))
        .map(|(i, _)| i)
        .nth(1)
        .expect("second call");
    assert!(choice_at < second_call_at);
}

/// A route recorded as ignoring a forced tool choice is not sent one: the
/// field would be an ignored parameter. The event says so and names the
/// record; the specific notice still goes out, and still works.
#[tokio::test]
async fn a_route_recorded_as_ignoring_tool_choice_gets_no_forced_choice() {
    let provider = Fake::new(true, |request| {
        if wrote(request) {
            says("Created Cargo.toml.")
        } else if told_specifically(request) {
            patch("Cargo.toml")
        } else {
            says(ANNOUNCEMENT)
        }
    });
    let record = "server source: the request type has no tool_choice field";
    let (events, result) = run(
        provider.clone(),
        ToolChoiceSupport::Ignored {
            record: record.into(),
        },
    )
    .await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert!(
        provider
            .requests()
            .iter()
            .all(|r| r.tool_choice == ToolChoice::Auto),
        "no request requires a tool call"
    );
    let choices = tool_choices(&events);
    assert_eq!(choices.len(), 1, "{choices:?}");
    assert!(!choices[0].0);
    assert!(
        choices[0].1.contains("recorded as ignoring it") && choices[0].1.contains(record),
        "{}",
        choices[0].1
    );
}

/// An adapter with no wire form is never sent one (it would refuse the
/// request), and the event says why.
#[tokio::test]
async fn an_adapter_with_no_wire_form_gets_no_forced_choice_and_says_so() {
    let provider = Fake::new(false, |request| {
        if wrote(request) {
            says("Created Cargo.toml.")
        } else if told_specifically(request) {
            patch("Cargo.toml")
        } else {
            says(ANNOUNCEMENT)
        }
    });
    let (events, result) = run(provider.clone(), ToolChoiceSupport::Unmeasured).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert!(
        provider
            .requests()
            .iter()
            .all(|r| r.tool_choice == ToolChoice::Auto)
    );
    let choices = tool_choices(&events);
    assert_eq!(choices.len(), 1, "{choices:?}");
    assert!(!choices[0].0);
    assert!(
        choices[0].1.contains("`fake` adapter has no wire form"),
        "{}",
        choices[0].1
    );
}

/// A model that never acts gets `MAX_NO_CHANGE_NOTICES` notices, each a
/// counted round with a forced choice; the last says it is the last; the
/// next end fails the attempt, naming the count and the writes.
#[tokio::test]
async fn the_notices_are_bounded_and_then_the_attempt_fails_with_a_named_reason() {
    assert_eq!(MAX_NO_CHANGE_NOTICES, 2);
    let provider = Fake::new(true, |_| says(ANNOUNCEMENT));
    let (events, result) = run(provider.clone(), ToolChoiceSupport::Unmeasured).await;
    let WorkerOutcome::Failed { error } = &result.outcome else {
        panic!("expected Failed, got {:?}", result.outcome);
    };
    assert!(
        error.contains(
            "the model ended the attempt as done again after being told to continue 2 time(s)"
        ),
        "{error}"
    );
    assert!(error.contains("Cargo.toml, wcl-core/Cargo.toml"), "{error}");
    let notices = no_change_notices(&events);
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(!notices[0].contains("last notice"));
    assert!(
        notices[1].contains("This is the last notice"),
        "{}",
        notices[1]
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "two notice rounds, no fourth");
    assert_eq!(requests[1].tool_choice, ToolChoice::Required);
    assert_eq!(requests[2].tool_choice, ToolChoice::Required);
    assert_eq!(tool_choices(&events).len(), 2);
    assert_eq!(result.steps, 3, "each notice round is counted");
}

/// A long announcement is cut in the quote, and the cut is marked.
#[tokio::test]
async fn a_long_announcement_is_quoted_in_part_with_the_cut_marked() {
    let long = format!("I'll write it. {}", "word ".repeat(200));
    let script_text = long.clone();
    let provider = Fake::new(true, move |_| says(&script_text));
    let (events, _) = run(provider, ToolChoiceSupport::Unmeasured).await;
    let notices = no_change_notices(&events);
    assert!(
        notices[0].contains("\"I'll write it. word"),
        "{}",
        notices[0]
    );
    assert!(notices[0].contains(" ...\""), "{}", notices[0]);
    assert!(notices[0].len() < long.len(), "{}", notices[0]);
}
