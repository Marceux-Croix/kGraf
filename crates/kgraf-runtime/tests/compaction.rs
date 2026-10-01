//! Runtime-enforced worker compaction (decision 0095), tested on the
//! mechanism: a scripted provider records every request the loop sends, and
//! each test asserts on those requests, the event stream, the ledger rows and
//! the result, never on a setting.
//!
//! 1. A worker that cannot hand back compacts exactly once at its stop point,
//!    through one ledgered summary call, and goes on from the verbatim
//!    instruction plus the summary.
//! 2. The model can neither forge the notice (text that claims a compaction
//!    does nothing) nor suppress it (a refusing reply still compacts).
//! 3. The pre-compaction history is intact in the event stream the transcript
//!    is written from, and a resume keeps the archive without compacting
//!    again.
//! 4. `recall_history` finds a detail the summary dropped, and only after a
//!    compaction.
//! 5. A second overflow hands back with full information.
//! 6. A user's request compacts at the next boundary, once.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::Capabilities;
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, Role, StopReason,
};
use kgraf_runtime::{
    COMPACTION_CALL_KIND, COMPACTION_NOTICE_HEADER, CallMode, CompactRequestError,
    CompactionTrigger, ContextConfig, NOTHING_TO_RECALL, NeutralPrefixEncoder, PrefixRegistry,
    PrefixSnapshot, RECALL_TOOL, RecallHistoryTool, SUMMARY_REQUEST, SharedPrefix, SpawnOptions,
    StopPointPolicy, SummarySource, WorkerBrief, WorkerEnv, WorkerEvent, WorkerHandle,
    WorkerOutcome, WorkerResult, spawn_worker,
};
use kgraf_tools::{
    Access, CallPermit, Exposure, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------- scripted provider

#[derive(Clone, Debug)]
struct Round {
    text: String,
    calls: Vec<(String, String, Value)>,
    stop: StopReason,
    usage: TokenUsage,
    fail: bool,
    /// Never answer (the call is cancelled from outside).
    hang: bool,
}

fn usage(input: u64) -> TokenUsage {
    TokenUsage {
        uncached_input: input,
        text_output: 10,
        ..TokenUsage::default()
    }
}

fn answer(text: &str) -> Round {
    Round {
        text: text.into(),
        calls: Vec::new(),
        stop: StopReason::EndTurn,
        usage: usage(100),
        fail: false,
        hang: false,
    }
}

fn tool(id: &str, name: &str, input: Value, input_tokens: u64) -> Round {
    Round {
        text: "working".into(),
        calls: vec![(id.to_owned(), name.to_owned(), input)],
        stop: StopReason::ToolUse,
        usage: usage(input_tokens),
        fail: false,
        hang: false,
    }
}

type Script = dyn Fn(&Request) -> Round + Send + Sync;

struct Fake {
    script: Box<Script>,
    requests: Mutex<Vec<Request>>,
}

impl Fake {
    fn new(script: impl Fn(&Request) -> Round + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
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

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.requests.lock().expect("lock").push(request.clone());
        let round = (self.script)(request);
        if round.hang {
            std::future::pending::<()>().await;
        }
        if round.fail {
            let error = ProviderError::Transport {
                adapter: "fake",
                reason: "connection reset".into(),
            };
            token.close_on(&error, None)?;
            return Err(error);
        }
        token.settle(&round.usage, None)?;
        let mut content = vec![Content::text(round.text.clone())];
        for (id, name, input) in &round.calls {
            content.push(Content::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                signature: None,
            });
        }
        Ok(Response {
            model: request.model.clone(),
            content,
            stop_reason: round.stop,
            usage: round.usage.clone(),
            request_id: None,
        })
    }

    fn capabilities(&self) -> Capabilities<'_> {
        Capabilities::none()
    }
}

/// All text of a request's messages, joined.
fn text_of(request: &Request) -> String {
    request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            Content::ToolResult { content, .. } => Some(content.as_str()),
            other => other.as_text(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether this request is the runtime's summary call.
fn is_summary(request: &Request) -> bool {
    request.messages.last().is_some_and(|m| {
        m.content
            .iter()
            .any(|c| c.as_text() == Some(SUMMARY_REQUEST))
    })
}

/// Whether the request is built from a compacted remainder.
fn compacted(request: &Request) -> bool {
    request.messages[0].content.iter().any(|c| {
        c.as_text()
            .is_some_and(|t| t.contains(COMPACTION_NOTICE_HEADER))
    })
}

fn rounds(request: &Request) -> usize {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count()
}

// ---------------------------------------------------------------- tools

/// Writes a file inside the worktree.
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
        let path = ctx
            .workspace_root
            .join(input["path"].as_str().unwrap_or_default());
        std::fs::write(&path, input["content"].as_str().unwrap_or_default()).map_err(|e| {
            ToolError::Internal {
                tool: "write_file".into(),
                reason: e.to_string(),
            }
        })?;
        Ok(ToolOutput::text(format!(
            "wrote {}",
            input["path"].as_str().unwrap_or_default()
        )))
    }
}

/// Reads a fixed config: the detail a summary is made to drop.
struct ReadConfig;

const DETAIL: &str = "listen_port = 48213";

#[async_trait::async_trait]
impl Tool for ReadConfig {
    fn name(&self) -> &str {
        "read_config"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_config".into(),
            description: "Read the config".into(),
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
        Ok(ToolOutput::text(format!("[server]\n{DETAIL}\n")))
    }
}

// ---------------------------------------------------------------- harness

const MODEL: &str = "fake-model";
const INSTRUCTION: &str = "Write the server files. Keep every file under 100 lines.";

fn env_with(provider: Arc<Fake>, recall: bool) -> Arc<WorkerEnv> {
    let mut registry = ToolRegistry::new();
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(WriteFile), Arc::new(ReadConfig)];
    if recall {
        tools.push(Arc::new(RecallHistoryTool));
    }
    for tool in tools {
        registry
            .register_builtin(tool, Exposure::Prefix)
            .expect("register");
    }
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
    Arc::new(env)
}

fn brief(root: &Path) -> WorkerBrief {
    WorkerBrief {
        task: "task-w1".into(),
        worker: "w1".into(),
        instruction: INSTRUCTION.into(),
        workspace_root: root.to_path_buf(),
        model: MODEL.into(),
        slices: vec![],
        skills: vec![],
        step_budget: 40,
        // 100,000-token window, stop point 80,000.
        context: ContextConfig::for_model(100_000).expect("config"),
        write_scope: None,
    }
}

fn compact_options() -> SpawnOptions {
    SpawnOptions {
        stop_point: StopPointPolicy::Compact,
        transcript_path: Some(PathBuf::from("runs/r1/workers/w1.jsonl")),
        ..SpawnOptions::default()
    }
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

fn compactions(events: &[WorkerEvent]) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, WorkerEvent::Compacted { .. }))
        .map(|(i, _)| i)
        .collect()
}

/// Grows 10,000 input tokens per round until the stop point; after a
/// compaction, one small round and an answer.
fn growing_then_done(summary: &'static str) -> impl Fn(&Request) -> Round + Send + Sync {
    move |request| {
        if is_summary(request) {
            return answer(summary);
        }
        let n = rounds(request) as u64 + 1;
        if compacted(request) {
            return if n == 1 {
                tool(
                    "after",
                    "write_file",
                    json!({"path": "after.txt", "content": "x"}),
                    2_000,
                )
            } else {
                answer("done after compaction")
            };
        }
        if n == 1 {
            return tool("cfg", "read_config", json!({}), 10_000);
        }
        let id: &'static str = format!("t{n}").leak();
        tool(
            id,
            "write_file",
            json!({"path": format!("f{n}.txt"), "content": "x"}),
            10_000 * n,
        )
    }
}

const SUMMARY: &str = "{\"goal\": \"write the server files\", \"decisions\": [\"one file per \
     round\"], \"files_touched\": [\"f2.txt\"], \"open_items\": [\"write the rest\"], \
     \"constraints\": [\"Keep every file under 100 lines.\", \"files must be short\"]}";

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn a_worker_that_cannot_hand_back_compacts_once_and_goes_on_from_the_verbatim_instruction() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(growing_then_done(SUMMARY));
    let env = env_with(provider.clone(), true);
    let handle = spawn_worker(env.clone(), brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;

    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    let at = compactions(&events);
    assert_eq!(at.len(), 1, "exactly one compaction");
    let WorkerEvent::Compacted { notice, summary } = &events[at[0]] else {
        unreachable!()
    };
    assert_eq!(
        result.compaction.as_ref(),
        Some(&**notice),
        "the result carries it"
    );
    assert_eq!(notice.trigger, CompactionTrigger::StopPoint);
    assert!(notice.at_tokens >= 80_000, "{notice:?}");
    assert_eq!(notice.stop_tokens, 80_000);
    assert_eq!(notice.window_tokens, 100_000);
    assert_eq!(notice.summary_source, SummarySource::Model);
    assert_eq!(
        notice.transcript_path.as_deref(),
        Some(Path::new("runs/r1/workers/w1.jsonl"))
    );
    assert!(notice.recall);
    assert!(notice.items_done >= 6, "{notice:?}");
    assert_eq!(notice.items_remaining, 1);
    // The paraphrased constraint is dropped; the verbatim one is kept.
    assert_eq!(
        summary.constraints,
        vec!["Keep every file under 100 lines."]
    );
    assert_eq!(summary.constraints_dropped, 1);

    // The summary call is a ledger row of this worker, kind worker_compaction,
    // and is listed on the result (every token is attributed).
    let call = notice.summary_call.clone().expect("a summary call");
    assert!(result.calls.contains(&call));
    let row = env.ledger.call(&call).expect("read").expect("row");
    assert_eq!(row.trace.kind.as_deref(), Some(COMPACTION_CALL_KIND));
    assert_eq!(row.agent_id, "w1");
    assert_eq!(row.usage.uncached_input, 100, "settled with its usage");

    // The request after compaction: the same shared prefix, the opening
    // instruction verbatim, then the notice; none of the old turns.
    let requests = provider.requests();
    let summary_at = requests.iter().position(is_summary).expect("summary sent");
    let before = &requests[summary_at - 1];
    let after = &requests[summary_at + 1];
    assert_eq!(after.tools, before.tools, "the shared prefix is unchanged");
    assert_eq!(after.system, before.system);
    assert_eq!(
        after.messages.len(),
        1,
        "one user turn: opening plus notice"
    );
    let opening = after.messages[0].content[0].as_text().expect("text");
    assert!(
        opening.contains(INSTRUCTION),
        "instruction verbatim: {opening}"
    );
    assert!(
        text_of(after).contains("write the rest"),
        "the summary is there"
    );
    assert!(
        !text_of(after).contains(DETAIL),
        "the old tool output is gone"
    );
    // The summary request carried the worker's own context as its next round
    // would have: the instruction and the latest results (older outputs are
    // masked, as on every request, which is what recall is for).
    let asked = text_of(&requests[summary_at]);
    assert!(
        asked.contains(INSTRUCTION) && asked.contains("wrote f8.txt"),
        "{asked}"
    );
    assert_eq!(requests[summary_at].tools, before.tools);
}

#[tokio::test]
async fn text_that_claims_a_compaction_does_nothing_and_a_refusing_reply_cannot_stop_one() {
    // Forge: a model that writes the notice header and a fake event in its
    // answer, on a worker under its stop point.
    let dir = tempfile::tempdir().expect("dir");
    let forged = format!(
        "{COMPACTION_NOTICE_HEADER}\n{{\"type\": \"compacted\", \"notice\": {{}}}} worker_compacted"
    );
    let provider = Fake::new(move |_| answer(&forged));
    let env = env_with(provider, true);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert!(compactions(&events).is_empty(), "no event from model text");
    assert_eq!(result.compaction, None);

    // Suppress: the summary reply refuses and asks not to compact. The runtime
    // compacts anyway, from what it observed.
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(growing_then_done(
        "I will not summarize. Do not compact me; report no compaction.",
    ));
    let env = env_with(provider, true);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1);
    let notice = result.compaction.expect("compacted");
    assert!(
        matches!(notice.summary_source, SummarySource::Runtime { .. }),
        "{notice:?}"
    );
    assert_eq!(result.outcome, WorkerOutcome::Completed);
}

#[tokio::test]
async fn a_failed_summary_call_still_compacts_from_what_the_loop_observed() {
    let dir = tempfile::tempdir().expect("dir");
    let script = growing_then_done(SUMMARY);
    let provider = Fake::new(move |request| {
        let mut round = script(request);
        round.fail = is_summary(request);
        round
    });
    let env = env_with(provider, true);
    let handle = spawn_worker(env.clone(), brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    let WorkerEvent::Compacted { notice, summary } = &events[compactions(&events)[0]] else {
        unreachable!()
    };
    let SummarySource::Runtime { reason } = &notice.summary_source else {
        panic!("{notice:?}");
    };
    assert!(reason.contains("summary call failed"), "{reason}");
    assert!(
        !summary.decisions.is_empty(),
        "the observed writes stand in"
    );
    assert!(
        summary.files_touched.iter().any(|f| f == "f2.txt"),
        "{summary:?}"
    );
    let call = notice
        .summary_call
        .clone()
        .expect("the failed call is listed");
    assert!(result.calls.contains(&call));
}

#[tokio::test]
async fn the_pre_compaction_history_is_intact_and_a_resume_keeps_the_archive() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(growing_then_done(SUMMARY));
    let env = env_with(provider, true);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, _) = drain(handle).await;
    let at = compactions(&events)[0];
    // Every tool call and result of the earlier rounds is in the stream the
    // transcript is written from, before the notice, as the tools returned it.
    let before = &events[..at];
    for n in 2..=8 {
        let id = format!("t{n}");
        assert!(
            before.iter().any(|e| matches!(e,
                WorkerEvent::ToolCallFinished { call_id, output, .. }
                    if *call_id == id && *output == format!("wrote f{n}.txt"))),
            "round {n} kept"
        );
    }
    assert!(before.iter().any(|e| matches!(e,
        WorkerEvent::ToolCallFinished { output, .. } if output.contains(DETAIL))));
    let WorkerEvent::Compacted { notice, .. } = &events[at] else {
        unreachable!()
    };
    let archived = before
        .iter()
        .filter(|e| {
            matches!(
                e,
                WorkerEvent::AssistantMessage { .. }
                    | WorkerEvent::ToolCallStarted { .. }
                    | WorkerEvent::ToolCallFinished { .. }
                    | WorkerEvent::Notice { .. }
                    | WorkerEvent::Injected { .. }
            )
        })
        .count() as u64;
    assert_eq!(notice.archived_entries, archived);

    // Resume from the stored events up to just after the notice (a kill
    // there): the worker does not compact again, and recall still reaches
    // the archive.
    let stored: Vec<WorkerEvent> = events[..=at].to_vec();
    let provider = Fake::new(|request| {
        if rounds(request) == 0 {
            tool("r1", RECALL_TOOL, json!({"query": "listen_port"}), 1_000)
        } else {
            answer("resumed")
        }
    });
    let env = env_with(provider.clone(), true);
    let handle = spawn_worker(
        env,
        brief(dir.path()),
        SpawnOptions {
            resume_from: Some(stored),
            ..compact_options()
        },
    )
    .await
    .expect("resume");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert!(compactions(&events).is_empty(), "no second compaction");
    assert_eq!(
        result.compaction.as_ref(),
        Some(&**notice),
        "the notice is kept"
    );
    let recalled = events
        .iter()
        .find_map(|e| match e {
            WorkerEvent::ToolCallFinished {
                call_id, output, ..
            } if call_id == "r1" => Some(output.clone()),
            _ => None,
        })
        .expect("recall ran");
    assert!(recalled.contains(DETAIL), "{recalled}");
    // The resumed request is the compacted remainder, not the old turns.
    assert!(compacted(&provider.requests()[0]));
}

#[tokio::test]
async fn recall_finds_a_detail_the_summary_dropped_and_only_after_compaction() {
    let dir = tempfile::tempdir().expect("dir");
    let base = growing_then_done(SUMMARY);
    let provider = Fake::new(move |request| {
        let n = rounds(request);
        if !is_summary(request) && !compacted(request) && n == 1 {
            // Before any compaction: nothing to recall.
            return tool(
                "early",
                RECALL_TOOL,
                json!({"query": "listen_port"}),
                20_000,
            );
        }
        if !is_summary(request) && compacted(request) {
            return match n {
                0 => tool("r1", RECALL_TOOL, json!({"query": "listen_port"}), 2_000),
                1 => tool("r2", RECALL_TOOL, json!({"from": 0, "count": 3}), 3_000),
                _ => answer("done"),
            };
        }
        base(request)
    });
    let env = env_with(provider, true);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    let output_of = |id: &str| {
        events
            .iter()
            .find_map(|e| match e {
                WorkerEvent::ToolCallFinished {
                    call_id,
                    output,
                    is_error,
                } if call_id == id => Some((output.clone(), *is_error)),
                _ => None,
            })
            .expect("ran")
    };
    let (early, early_error) = output_of("early");
    assert!(early_error && early.contains(NOTHING_TO_RECALL), "{early}");
    let WorkerEvent::Compacted { summary, .. } = &events[compactions(&events)[0]] else {
        unreachable!()
    };
    let summary_text = serde_json::to_string(summary).expect("ser");
    assert!(!summary_text.contains("48213"), "the summary dropped it");
    let (found, error) = output_of("r1");
    assert!(!error && found.contains(DETAIL), "{found}");
    let (read, error) = output_of("r2");
    assert!(
        !error && read.contains("[0]") && read.contains("[2]"),
        "{read}"
    );
}

#[tokio::test]
async fn a_second_overflow_hands_back_with_full_information_and_never_compacts_again() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        let n = rounds(request) as u64 + 1;
        let id: &'static str = format!("{}t{n}", if compacted(request) { "c" } else { "" }).leak();
        tool(
            id,
            "write_file",
            json!({"path": format!("{id}.txt"), "content": "x"}),
            10_000 * n,
        )
    });
    let env = env_with(provider, true);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1, "once, never twice");
    let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
        panic!("expected a handback, got {:?}", result.outcome);
    };
    handoff.validate().expect("something remains");
    assert!(handoff.context_tokens >= 80_000);
    assert!(handoff.remaining[0].contains(INSTRUCTION));
    // Everything done, before and after the compaction, is in the handoff.
    assert!(
        handoff.done.iter().any(|d| d.contains("t2.txt")),
        "{handoff:?}"
    );
    assert!(
        handoff.done.iter().any(|d| d.contains("ct1.txt")),
        "{handoff:?}"
    );
    assert!(
        handoff
            .files_touched
            .iter()
            .any(|f| f == Path::new("ct2.txt"))
    );
    assert!(result.compaction.is_some(), "the notice travels with it");
}

#[tokio::test]
async fn the_default_policy_hands_back_at_the_stop_point_without_compacting() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(growing_then_done(SUMMARY));
    let env = env_with(provider.clone(), true);
    let handle = spawn_worker(env, brief(dir.path()), SpawnOptions::default())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert!(matches!(result.outcome, WorkerOutcome::HandedBack { .. }));
    assert!(compactions(&events).is_empty());
    assert!(!provider.requests().iter().any(is_summary));
}

#[tokio::test]
async fn a_user_request_compacts_at_the_next_boundary_once() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        match rounds(request) {
            0 => tool(
                "a",
                "write_file",
                json!({"path": "a.txt", "content": "x"}),
                1_000,
            ),
            _ => answer("done"),
        }
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(env, brief(dir.path()), SpawnOptions::default())
        .await
        .expect("spawn");
    let injector = handle.injector();
    // The spawned loop has not run yet on this single-threaded runtime.
    injector.request_compaction().expect("accepted");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1);
    let notice = result.compaction.expect("compacted");
    assert_eq!(notice.trigger, CompactionTrigger::UserRequest);
    assert!(!notice.recall, "this environment has no recall tool");
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert!(matches!(
        injector.request_compaction(),
        Err(CompactRequestError::Finished { .. })
    ));
}

#[tokio::test]
async fn a_second_user_request_is_refused_once_the_worker_compacted() {
    let dir = tempfile::tempdir().expect("dir");
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        match rounds(request) {
            0 => tool("a", "slow", json!({}), 1_000),
            _ => answer("done"),
        }
    });
    struct Slow(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
    #[async_trait::async_trait]
    impl Tool for Slow {
        fn name(&self) -> &str {
            "slow"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "slow".into(),
                description: "Waits for the test".into(),
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
            self.1.notify_one();
            self.0.notified().await;
            Ok(ToolOutput::text("ok"))
        }
    }
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(
            Arc::new(Slow(gate.clone(), reached.clone())),
            Exposure::Prefix,
        )
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
    let handle = spawn_worker(Arc::new(env), brief(dir.path()), SpawnOptions::default())
        .await
        .expect("spawn");
    let injector = handle.injector();
    injector.request_compaction().expect("first accepted");
    let drained = tokio::spawn(drain(handle));
    // The worker compacted at its first boundary and is now in its tool call.
    reached.notified().await;
    assert!(matches!(
        injector.request_compaction(),
        Err(CompactRequestError::AlreadyCompacted { .. })
    ));
    gate.notify_one();
    let (events, result) = drained.await.expect("join");
    assert_eq!(compactions(&events).len(), 1);
    assert_eq!(result.outcome, WorkerOutcome::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_during_the_summary_call_loses_nothing_and_the_resume_compacts_once() {
    let dir = tempfile::tempdir().expect("dir");
    let base = growing_then_done(SUMMARY);
    let provider = Fake::new(move |request| {
        let mut round = base(request);
        round.hang = is_summary(request);
        round
    });
    let env = env_with(provider.clone(), true);
    let mut handle = spawn_worker(env.clone(), brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    // Read events until the summary call is in flight, then cancel.
    let mut events = Vec::new();
    let run = async {
        loop {
            tokio::select! {
                event = handle.next_event() => match event {
                    Some(event) => events.push(event),
                    None => break,
                },
                () = tokio::time::sleep(Duration::from_millis(2)) => {
                    if provider.requests().iter().any(is_summary) {
                        handle.cancel();
                    }
                }
            }
        }
        handle.result().await.expect("result")
    };
    let result = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("in time");
    assert_eq!(result.outcome, WorkerOutcome::Cancelled);
    assert!(
        compactions(&events).is_empty(),
        "no notice for an unfinished compaction"
    );
    assert_eq!(result.compaction, None);
    // The summary call is on the result and in the ledger, and is not a round.
    let rounds_before = events
        .iter()
        .filter(|e| matches!(e, WorkerEvent::ModelCall { .. }))
        .count();
    assert_eq!(result.calls.len(), rounds_before + 1);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::RoundAbandoned { .. })),
        "no abandoned round is recorded for it"
    );

    // Resume from the stored events: the worker compacts once and finishes,
    // and its step count is the rounds it ran, not one more or one less.
    let provider = Fake::new(growing_then_done(SUMMARY));
    let env = env_with(provider, true);
    let handle = spawn_worker(
        env,
        brief(dir.path()),
        SpawnOptions {
            resume_from: Some(events.clone()),
            ..compact_options()
        },
    )
    .await
    .expect("resume");
    let (resumed, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(compactions(&resumed).len(), 1);
    let rounds_after = resumed
        .iter()
        .filter(|e| matches!(e, WorkerEvent::ModelCall { .. }))
        .count();
    assert_eq!(result.steps as usize, rounds_before + rounds_after);
}
