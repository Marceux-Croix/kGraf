//! Runtime-enforced worker compaction (decision 0095), tested on the
//! mechanism: a scripted provider records every request the loop sends, and
//! each test asserts on those requests, the event stream, the ledger rows and
//! the result, never on a setting.
//!
//! 1. A worker that cannot hand back compacts at its stop point,
//!    through one ledgered summary call, and goes on from the verbatim
//!    instruction plus the summary.
//! 2. The model can neither forge the notice (text that claims a compaction
//!    does nothing) nor suppress it (a refusing reply still compacts).
//! 3. The pre-compaction history is intact in the event stream the transcript
//!    is written from, and a resume keeps the archive without compacting
//!    again.
//! 4. `recall_history` finds a detail the summary dropped, and only after a
//!    compaction.
//! 5. A later overflow compacts again only on measured progress, with room
//!    under the stop point, up to the checkpoint ceiling; otherwise it hands
//!    back with full information and the reason.
//! 6. A user's request compacts at the next boundary, once.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::Capabilities;
use kgraf_provider::message::ToolDefinition;
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, Request, Response, Role, StopReason,
};
use kgraf_runtime::{
    COMPACTION_CALL_KIND, COMPACTION_NOTICE_HEADER, CallMode, CompactRequestError,
    CompactionTrigger, ContextConfig, NOTHING_TO_RECALL, NeutralPrefixEncoder, PointSource,
    PrefixRegistry, PrefixSnapshot, RECALL_TOOL, RecallHistoryTool, SUMMARY_REQUEST, SharedPrefix,
    SpawnOptions, StopPointPolicy, SummarySource, WorkerBrief, WorkerEnv, WorkerEvent,
    WorkerHandle, WorkerOutcome, WorkerResult, spawn_worker,
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
            finish_reason: None,
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
        // A patch that does not match: the file is left as it was.
        if input["fail"].as_bool() == Some(true) {
            return Err(ToolError::Internal {
                tool: "write_file".into(),
                reason: "expected lines not found".into(),
            });
        }
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
    let WorkerEvent::Compacted {
        notice, summary, ..
    } = &events[at[0]]
    else {
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
    let WorkerEvent::Compacted {
        notice, summary, ..
    } = &events[compactions(&events)[0]]
    else {
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

// ------------------------------------------- a later stop point (0095, 2026-10-06)

/// A later summary in other words than the first: the first summary's decision
/// must survive it word for word.
const LATER_SUMMARY: &str = "{\"goal\": \"write the server files\", \"decisions\": [\"later \
     decision\"], \"files_touched\": [], \"open_items\": [\"write the rest\"], \
     \"constraints\": []}";

/// `env_with`, with the progress checkpoint ceiling set to `ceiling`.
fn env_with_ceiling(provider: Arc<Fake>, ceiling: u32) -> Arc<WorkerEnv> {
    let mut env = Arc::try_unwrap(env_with(provider, true))
        .ok()
        .expect("one owner");
    env.settings.max_checkpoints = ceiling;
    Arc::new(env)
}

/// Writes a new file every round and grows 10,000 input tokens per round of
/// its current context, so it reaches the 80,000-token stop point in round 8
/// of every segment. The first summary differs from the later ones. File
/// names count up from `first + 1`.
fn writes_every_round(first: u64) -> impl Fn(&Request) -> Round + Send + Sync {
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(first));
    move |request| {
        if is_summary(request) {
            return answer(if compacted(request) {
                LATER_SUMMARY
            } else {
                SUMMARY
            });
        }
        let k = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let n = rounds(request) as u64 + 1;
        let id: &'static str = format!("w{k}").leak();
        tool(
            id,
            "write_file",
            json!({"path": format!("{id}.txt"), "content": "x"}),
            10_000 * n,
        )
    }
}

fn notices_of(events: &[WorkerEvent]) -> Vec<(kgraf_runtime::CompactionNotice, Vec<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Compacted {
                notice, summary, ..
            } => Some(((**notice).clone(), summary.decisions.clone())),
            _ => None,
        })
        .collect()
}

/// A worker that cannot hand back and keeps writing reaches its stop point
/// again after each compaction. It compacts again each time, because the
/// rounds since made measured progress, until the checkpoint ceiling; then it
/// hands back with everything it did and the reason. Every later summary keeps
/// the first one's decision word for word.
#[tokio::test]
async fn a_later_stop_point_with_measured_progress_compacts_again_up_to_the_ceiling() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(writes_every_round(0));
    let env = env_with_ceiling(provider.clone(), 3);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    let notices = notices_of(&events);
    assert_eq!(
        notices.len(),
        3,
        "compacted at each stop point up to the ceiling"
    );
    assert_eq!(
        notices.iter().map(|(n, _)| n.number).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        notices
            .iter()
            .map(|(n, _)| n.may_compact_again)
            .collect::<Vec<_>>(),
        vec![true, true, false],
        "the last one below the ceiling says it is the last"
    );
    // Each archive holds everything before its notice, so it only grows.
    assert!(
        notices
            .windows(2)
            .all(|w| w[0].0.archived_entries < w[1].0.archived_entries),
        "{notices:?}"
    );
    // The first summary's decision survives the later summaries.
    for (_, decisions) in &notices[1..] {
        assert!(
            decisions.iter().any(|d| d == "one file per round"),
            "{decisions:?}"
        );
        assert!(decisions.iter().any(|d| d == "later decision"));
    }
    // The model is told which rule applies at its next limit.
    let after: Vec<String> = provider
        .requests()
        .iter()
        .filter(|r| compacted(r) && !is_summary(r) && rounds(r) == 0)
        .map(text_of)
        .collect();
    assert_eq!(after.len(), 3);
    assert!(after[0].contains("compacted again only if your files or checks show progress"));
    assert!(!after[2].contains("compacted again only if"));
    let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
        panic!("expected a hand-back, got {:?}", result.outcome);
    };
    assert_eq!(
        handoff.compaction_stop,
        Some(kgraf_runtime::CompactionStop::Ceiling {
            compactions: 3,
            ceiling: 3
        })
    );
    handoff.validate().expect("something remains");
    assert!(handoff.context_tokens >= 80_000);
    assert!(handoff.remaining[0].contains(INSTRUCTION));
    // Everything done, in every segment, is in the handoff.
    assert!(
        handoff.done.iter().any(|d| d.contains("w1.txt")),
        "{handoff:?}"
    );
    assert!(handoff.done.len() >= 30, "{}", handoff.done.len());
    assert_eq!(result.compaction.as_ref().map(|n| n.number), Some(1));
    assert_eq!(result.later_compactions.len(), 2);
}

/// After a compaction the worker only reads. At its next stop point the rounds
/// since the compaction show no measured progress, so it is not compacted
/// again: it hands back, with what was measured.
#[tokio::test]
async fn a_later_stop_point_with_no_progress_since_the_compaction_hands_back_and_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        let n = rounds(request) as u64 + 1;
        if compacted(request) {
            let id: &'static str = format!("r{n}").leak();
            return tool(id, "read_config", json!({"section": n}), 10_000 * n);
        }
        let id: &'static str = format!("t{n}").leak();
        tool(
            id,
            "write_file",
            json!({"path": format!("f{n}.txt"), "content": "x"}),
            10_000 * n,
        )
    });
    let env = env_with(provider.clone(), true);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1, "no second compaction");
    let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
        panic!("expected a hand-back, got {:?}", result.outcome);
    };
    let Some(kgraf_runtime::CompactionStop::NoProgress {
        compactions,
        measure,
    }) = &handoff.compaction_stop
    else {
        panic!("expected no progress, got {:?}", handoff.compaction_stop);
    };
    assert_eq!(*compactions, 1);
    assert!(measure.files_changed.is_empty(), "{measure:?}");
    assert!(measure.checks_improved.is_empty());
    assert!(measure.rounds >= 7, "{measure:?}");
    let said = handoff.compaction_stop.as_ref().expect("stop").describe();
    assert!(
        said.contains("no measured progress since compaction 1"),
        "{said}"
    );
    // The work before the compaction is in the handoff.
    assert!(
        handoff.done.iter().any(|d| d.contains("f2.txt")),
        "{handoff:?}"
    );
}

/// After a compaction the worker fails three writes of one file with one
/// identical error, then writes that file whole, then only reads. Its later stop point is a
/// progressing segment: the failed writes came before a write that works, so
/// they are not a sloppy pattern, and it is compacted again (here up to a
/// ceiling of 2). Before this rule the segment read "no measured progress"
/// with the changed file in the same sentence (a worker that replaced its
/// stub module whole after three refused patches).
#[tokio::test]
async fn failed_writes_before_a_write_that_works_do_not_stop_a_recompaction() {
    let dir = tempfile::tempdir().expect("dir");
    // Each segment's whole-module write differs from the last one's.
    let segment = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let provider = Fake::new(move |request| {
        if is_summary(request) {
            return answer(if compacted(request) {
                LATER_SUMMARY
            } else {
                SUMMARY
            });
        }
        let n = rounds(request) as u64 + 1;
        let tag = if compacted(request) { "c" } else { "t" };
        let id: &'static str = format!("{tag}{n}").leak();
        if !compacted(request) {
            return tool(
                id,
                "write_file",
                json!({"path": format!("f{n}.txt"), "content": "x"}),
                10_000 * n,
            );
        }
        match n {
            // Three patches of the module that do not match: the same error
            // each time.
            1..=3 => tool(
                id,
                "write_file",
                json!({"path": "module.txt", "content": format!("hunk {n}"), "fail": true}),
                10_000 * n,
            ),
            4 => tool(
                id,
                "write_file",
                json!({
                    "path": "module.txt",
                    "content": format!(
                        "whole module {}",
                        segment.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    )
                }),
                10_000 * n,
            ),
            _ => tool(id, "read_config", json!({"section": n}), 10_000 * n),
        }
    });
    let env = env_with_ceiling(provider.clone(), 2);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(
        compactions(&events).len(),
        2,
        "the segment with the recovered write compacted again"
    );
    let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
        panic!("expected a hand-back, got {:?}", result.outcome);
    };
    assert_eq!(
        handoff.compaction_stop,
        Some(kgraf_runtime::CompactionStop::Ceiling {
            compactions: 2,
            ceiling: 2
        }),
        "{:?}",
        handoff.compaction_stop
    );
}

/// The opening message and the summary alone reach the stop point after the
/// compaction: no compaction can make room, so the worker hands back at once,
/// before another round, and says so with the numbers.
#[tokio::test]
async fn a_compaction_that_cannot_get_under_the_stop_point_hands_back_at_once() {
    let dir = tempfile::tempdir().expect("dir");
    // About 60,000 tokens of instruction (3 bytes a token) under a stop point
    // of 80,000, and a summary of about 25,000 more.
    let filler = "keep the files short. ".repeat(8_200);
    let big_summary: &'static str = format!(
        "{{\"goal\": \"write the server files\", \"decisions\": [\"{}\"], \"files_touched\": [], \
         \"open_items\": [\"write the rest\"], \"constraints\": []}}",
        "one file per round, ".repeat(3_700)
    )
    .leak();
    let provider = Fake::new(move |request| {
        if is_summary(request) {
            return answer(big_summary);
        }
        if compacted(request) {
            return answer("a round after a compaction that left no room");
        }
        tool(
            "t1",
            "write_file",
            json!({"path": "f1.txt", "content": "x"}),
            85_000,
        )
    });
    let env = env_with(provider.clone(), true);
    let mut b = brief(dir.path());
    b.instruction = format!("{INSTRUCTION}\n{filler}");
    let handle = spawn_worker(env, b, compact_options())
        .await
        .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1);
    let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
        panic!("expected a hand-back, got {:?}", result.outcome);
    };
    let Some(kgraf_runtime::CompactionStop::NoRoom {
        compactions,
        floor_tokens,
        stop_tokens,
    }) = &handoff.compaction_stop
    else {
        panic!("expected no room, got {:?}", handoff.compaction_stop);
    };
    assert_eq!((*compactions, *stop_tokens), (1, 80_000));
    assert!(*floor_tokens >= 80_000, "{floor_tokens}");
    assert!(
        !provider
            .requests()
            .iter()
            .any(|r| compacted(r) && !is_summary(r)),
        "no round ran after the compaction"
    );
    let said = handoff.compaction_stop.as_ref().expect("stop").describe();
    assert!(said.contains("compaction cannot make room"), "{said}");
}

/// A resume right after a compaction measures the progress since it from the
/// baseline the `Compacted` event stored, so the resumed worker that keeps
/// writing compacts again at its next stop point.
#[tokio::test]
async fn a_resume_after_a_compaction_measures_progress_from_the_stored_baseline() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(writes_every_round(0));
    let env = env_with_ceiling(provider, 3);
    let handle = spawn_worker(env, brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (events, _) = drain(handle).await;
    let at = compactions(&events)[0];
    let WorkerEvent::Compacted { baseline, .. } = &events[at] else {
        unreachable!()
    };
    assert!(baseline.is_some(), "the baseline is stored on the event");
    let stored: Vec<WorkerEvent> = events[..=at].to_vec();
    // New files after the resume: rewriting the same files with the same
    // content would be no net change.
    let provider = Fake::new(writes_every_round(100));
    let env = env_with_ceiling(provider, 2);
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
    let notices = notices_of(&events);
    assert_eq!(
        notices.len(),
        1,
        "one more compaction after the resume: {:?}",
        result.outcome
    );
    assert_eq!(notices[0].0.number, 2);
    let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
        panic!("expected a hand-back, got {:?}", result.outcome);
    };
    assert_eq!(
        handoff.compaction_stop,
        Some(kgraf_runtime::CompactionStop::Ceiling {
            compactions: 2,
            ceiling: 2
        })
    );
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

// ------------------------------------------------ no-change ends and early hand-backs

/// A worker whose context window is 32,768 tokens (stop point 11,468), as the
/// local model in the evidence that drove these tests.
fn small_window_brief(root: &Path, writes: Option<Vec<PathBuf>>) -> WorkerBrief {
    WorkerBrief {
        context: ContextConfig::for_model(32_768)
            .expect("config")
            .with_point(35, PointSource::User)
            .expect("point"),
        write_scope: writes,
        ..brief(root)
    }
}

/// One reply that reports `input` prompt tokens and `output` generated tokens
/// and calls `read_config`: a long first reply plus one read.
fn long_first_reply(n: u64, input: u64, output: u64) -> Round {
    // `n` varies the call, so repeating it is not a stall.
    let mut round = tool(&format!("cfg{n}"), "read_config", json!({"n": n}), input);
    round.usage.text_output = output;
    round
}

fn notices(events: &[WorkerEvent], kind: kgraf_runtime::NoticeKind) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, WorkerEvent::Notice { kind: k, .. } if *k == kind))
        .count()
}

#[tokio::test]
async fn an_announcement_with_no_tool_call_is_not_a_completion_when_the_contract_writes() {
    let dir = tempfile::tempdir().expect("dir");
    // The first turn is dropped from the transcript, so the second request
    // looks like the first: the script counts calls instead.
    let sent = std::sync::atomic::AtomicUsize::new(0);
    let provider =
        Fake::new(
            move |_| match sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => {
                    answer("I'll implement all 8 classes in `shapes/library.py` and run the tests.")
                }
                1 => tool(
                    "w",
                    "write_file",
                    json!({"path": "shapes.py", "content": "x"}),
                    200,
                ),
                _ => answer("done"),
            },
        );
    let env = env_with(provider.clone(), false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("shapes.py")])),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(result.steps, 3, "the notice round is counted");
    assert_eq!(notices(&events, kgraf_runtime::NoticeKind::NoChange), 1);
    let requests = provider.requests();
    let text = text_of(&requests[1]);
    assert!(
        text.contains("You ended without making any change; this contract writes shapes.py."),
        "{text}"
    );
    assert!(text.contains("report_shortfall"), "{text}");
    // The dropped turn is not resent as the model's own turn; the notice
    // quotes it, so the model sees what it said it would do.
    assert!(
        requests[1]
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.content.iter())
            .all(|c| !c
                .as_text()
                .is_some_and(|t| t.contains("I'll implement all 8 classes"))),
        "the dropped turn is not resent"
    );
    assert!(
        text.contains(
            "Your reply said: \"I'll implement all 8 classes in `shapes/library.py` and run the \
             tests.\""
        ),
        "{text}"
    );
}

#[tokio::test]
async fn a_second_end_with_no_tool_call_fails_the_attempt_with_a_named_reason() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|_| answer("I'll do it now."));
    let env = env_with(provider.clone(), false);
    let handle = spawn_worker(
        env,
        small_window_brief(
            dir.path(),
            Some(vec![PathBuf::from("a.py"), PathBuf::from("b.py")]),
        ),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = drain(handle).await;
    let WorkerOutcome::Failed { error } = &result.outcome else {
        panic!("expected Failed, got {:?}", result.outcome);
    };
    assert!(
        error.contains("no write and no mutating command"),
        "{error}"
    );
    assert!(
        error.contains("a.py, b.py"),
        "the writes are named: {error}"
    );
    assert_eq!(
        provider.requests().len(),
        1 + kgraf_runtime::MAX_NO_CHANGE_NOTICES as usize,
        "one round per notice, no round past the bound"
    );
}

#[tokio::test]
async fn an_answer_with_no_tool_call_still_completes_a_contract_that_writes_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    for scope in [None, Some(Vec::new())] {
        let provider = Fake::new(|_| answer("The answer is 4."));
        let env = env_with(provider.clone(), false);
        let handle = spawn_worker(
            env,
            small_window_brief(dir.path(), scope),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let (events, result) = drain(handle).await;
        assert_eq!(result.outcome, WorkerOutcome::Completed);
        assert_eq!(notices(&events, kgraf_runtime::NoticeKind::NoChange), 0);
        assert_eq!(provider.requests().len(), 1);
    }
}

/// Round evidence (polyglot attempt 2): two `read_file` calls, then the turn
/// ended as done with nothing written, and the attempt was recorded completed.
/// A read is not a change, so the contract that writes gets the notices, and
/// a read-only end past their bound fails the attempt.
#[tokio::test]
async fn a_read_only_attempt_that_used_a_tool_is_not_a_completion_when_the_contract_writes() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| match rounds(request) {
        0 => tool("cfg", "read_config", json!({}), 200),
        _ => answer("the config is fine; nothing needed changing"),
    });
    let env = env_with(provider.clone(), false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("a.py")])),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(
        notices(&events, kgraf_runtime::NoticeKind::NoChange),
        kgraf_runtime::MAX_NO_CHANGE_NOTICES as usize
    );
    let WorkerOutcome::Failed { error } = &result.outcome else {
        panic!("expected Failed, got {:?}", result.outcome);
    };
    assert!(
        error.contains("no write and no mutating command"),
        "{error}"
    );
    assert!(error.contains("a.py"), "{error}");
}

/// A read followed by a write is a completion for a contract that writes.
#[tokio::test]
async fn a_read_then_a_write_completes_without_a_notice() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| match rounds(request) {
        0 => tool("cfg", "read_config", json!({}), 200),
        1 => tool(
            "w",
            "write_file",
            json!({"path": "a.py", "content": "x"}),
            200,
        ),
        _ => answer("done"),
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("a.py")])),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(notices(&events, kgraf_runtime::NoticeKind::NoChange), 0);
}

/// A read-only attempt is still a completion for a contract that writes nothing.
#[tokio::test]
async fn a_read_only_attempt_that_used_a_tool_completes_a_contract_that_writes_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| match rounds(request) {
        0 => tool("cfg", "read_config", json!({}), 200),
        _ => answer("the config is fine"),
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), None),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed);
    assert_eq!(notices(&events, kgraf_runtime::NoticeKind::NoChange), 0);
}

/// The evidence shape: one reply of 6,319 prompt and a long generated part,
/// one read, nothing written; the measured context passes the stop point
/// (11,468). Handing the whole task to a child would repeat it, so the worker
/// compacts and goes on.
#[tokio::test]
async fn a_stop_point_reached_with_nothing_written_compacts_instead_of_handing_back() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        if compacted(request) {
            return match rounds(request) {
                0 => tool(
                    "w",
                    "write_file",
                    json!({"path": "shapes.py", "content": "x"}),
                    3_000,
                ),
                _ => answer("done"),
            };
        }
        long_first_reply(rounds(request) as u64, 6_319, 5_300)
    });
    let env = env_with(provider.clone(), false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), None),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(compactions(&events).len(), 1);
    let notice = result.compaction.expect("compacted");
    assert_eq!(notice.trigger, CompactionTrigger::StopPoint);
    assert_eq!(notice.stop_tokens, 11_468);
    assert!(notice.at_tokens >= 11_468, "{notice:?}");
}

/// After that one compaction, a second stop point with still nothing written
/// does not hand the whole task back either, while the window holds.
#[tokio::test]
async fn a_second_stop_point_with_nothing_written_goes_on_while_the_window_holds() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        if compacted(request) {
            return match rounds(request) {
                0 => long_first_reply(1, 12_000, 100),
                _ => answer("done"),
            };
        }
        long_first_reply(rounds(request) as u64, 6_319, 5_300)
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), None),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert_eq!(compactions(&events).len(), 1, "compacted once, never twice");
}

/// With nothing written and the window itself passed, the hand-back stays: the
/// next request would not fit. It names what was measured.
#[tokio::test]
async fn a_hand_back_with_nothing_written_past_the_window_names_the_measured_context() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        if compacted(request) {
            return long_first_reply(1, 40_000, 100);
        }
        long_first_reply(rounds(request) as u64, 6_319, 5_300)
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), None),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (_, result) = drain(handle).await;
    let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
        panic!("expected a hand-back, got {:?}", result.outcome);
    };
    assert_eq!(handoff.cause, kgraf_runtime::HandoffCause::MeasuredContext);
    assert!(handoff.context_tokens >= 40_000);
}

/// Work done, then the stop point: the hand-back stands, and says it came
/// from measured context.
#[tokio::test]
async fn a_hand_back_after_a_write_stands_and_names_the_measured_context() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(growing_then_done(SUMMARY));
    let env = env_with(provider, true);
    let handle = spawn_worker(env, brief(dir.path()), SpawnOptions::default())
        .await
        .expect("spawn");
    let (_, result) = drain(handle).await;
    let WorkerOutcome::HandedBack { handoff } = &result.outcome else {
        panic!("expected a hand-back, got {:?}", result.outcome);
    };
    assert!(!handoff.done.is_empty());
    assert_eq!(handoff.cause, kgraf_runtime::HandoffCause::MeasuredContext);
}

// ------------------------------------------------ state across a compaction

/// Nothing written, the stop point reached, a compaction, two reads, then an
/// end as done with no write. The no-change guard holds after the
/// compaction, so the worker is told once and then writes.
#[tokio::test]
async fn the_no_change_guard_holds_after_a_compaction_with_nothing_written() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        if compacted(request) {
            let told = text_of(request).contains("You ended without making any change");
            return match (rounds(request), told) {
                (0, _) => tool("after-a", "read_config", json!({"n": "a"}), 3_000),
                (1, _) => tool("after-b", "read_config", json!({"n": "b"}), 3_200),
                (2, false) => answer("I'll implement all three containers now."),
                (2, true) => tool(
                    "w",
                    "write_file",
                    json!({"path": "shapes.py", "content": "x"}),
                    3_400,
                ),
                _ => answer("done"),
            };
        }
        long_first_reply(rounds(request) as u64, 6_319, 5_300)
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("shapes.py")])),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1);
    assert_eq!(
        notices(&events, kgraf_runtime::NoticeKind::NoChange),
        1,
        "the end with nothing written after the compaction is not a completion"
    );
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    assert!(dir.path().join("shapes.py").exists());
}

/// The guard does not catch a worker that wrote before its compaction: what
/// it wrote carries across, so an end as done after the compaction completes.
#[tokio::test]
async fn a_write_before_the_compaction_still_counts_after_it() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        if compacted(request) {
            return answer("done: shapes.py is written");
        }
        tool(
            "w",
            "write_file",
            json!({"path": "shapes.py", "content": "x"}),
            12_000,
        )
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("shapes.py")])),
        compact_options(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1);
    assert_eq!(notices(&events, kgraf_runtime::NoticeKind::NoChange), 0);
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
}

/// Two identical reads before the compaction, then `after` identical reads
/// after it, a write and an answer. The second read crosses the stop point
/// (11,468) with nothing written, so the worker compacts there.
fn reads_around_a_compaction(after: usize) -> impl Fn(&Request) -> Round + Send + Sync {
    move |request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        let n = rounds(request);
        if compacted(request) {
            if n < after {
                let id: &'static str = format!("again{n}").leak();
                return tool(id, "read_config", json!({}), 3_000 + n as u64 * 100);
            }
            return if n == after {
                tool(
                    "w",
                    "write_file",
                    json!({"path": "shapes.py", "content": "x"}),
                    3_500,
                )
            } else {
                answer("done")
            };
        }
        let id: &'static str = format!("before{n}").leak();
        tool(id, "read_config", json!({}), 6_000 * (n as u64 + 1))
    }
}

/// Two identical reads before the compaction and one after it. The earlier
/// results are gone from the model's context, so the re-read is not a stall;
/// the count starts again at the compaction.
#[tokio::test]
async fn a_re_read_after_a_compaction_is_not_a_stall() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(reads_around_a_compaction(1));
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("shapes.py")])),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1);
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
}

/// A call repeated to the threshold after the compaction still stalls, at its
/// third issue since the compaction.
#[tokio::test]
async fn a_real_repeat_after_a_compaction_still_stalls() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(reads_around_a_compaction(10));
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("shapes.py")])),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1);
    assert!(
        matches!(result.outcome, WorkerOutcome::Stalled { repeats: 3, .. }),
        "{result:?}"
    );
    let after = compactions(&events)[0];
    let issued_after = events[after..]
        .iter()
        .filter(|e| matches!(e, WorkerEvent::ToolCallStarted { tool, .. } if tool == "read_config"))
        .count();
    assert_eq!(issued_after, 3, "{events:?}");
}

/// A resume after the compaction counts from the compaction too: the replay
/// resets where the live loop did.
#[tokio::test]
async fn a_resume_after_a_compaction_counts_repeats_from_the_compaction() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(reads_around_a_compaction(1));
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("shapes.py")])),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, _) = drain(handle).await;
    // Stored up to the first read after the compaction, its result included.
    let cut = events
        .iter()
        .position(
            |e| matches!(e, WorkerEvent::ToolCallFinished { call_id, .. } if call_id == "again0"),
        )
        .expect("the read after the compaction ran");
    let stored: Vec<WorkerEvent> = events[..=cut].to_vec();
    // The resumed worker reads once more (the second issue since the
    // compaction, the fourth in all), then writes.
    let provider = Fake::new(|request| match rounds(request) {
        1 => tool("again1", "read_config", json!({}), 3_200),
        2 => tool(
            "w",
            "write_file",
            json!({"path": "shapes.py", "content": "x"}),
            3_500,
        ),
        _ => answer("done"),
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(
        env,
        small_window_brief(dir.path(), Some(vec![PathBuf::from("shapes.py")])),
        SpawnOptions {
            resume_from: Some(stored),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("resume");
    let (_, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
}

/// A summary reply out of schema still carries the goal and the unwritten
/// write paths, from kGraf's own state.
#[tokio::test]
async fn a_runtime_summary_carries_the_goal_and_the_unwritten_paths() {
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(|request| {
        if is_summary(request) {
            return answer("{\"goal\": \"implement\", \"decisions\": [\"a\" \"b\"]}");
        }
        if compacted(request) {
            return match rounds(request) {
                0 => tool(
                    "w",
                    "write_file",
                    json!({"path": "a.py", "content": "x"}),
                    3_000,
                ),
                1 => tool(
                    "w2",
                    "write_file",
                    json!({"path": "b.py", "content": "x"}),
                    3_100,
                ),
                _ => answer("done"),
            };
        }
        long_first_reply(rounds(request) as u64, 6_319, 5_300)
    });
    let env = env_with(provider.clone(), false);
    let handle = spawn_worker(
        env,
        small_window_brief(
            dir.path(),
            Some(vec![PathBuf::from("a.py"), PathBuf::from("b.py")]),
        ),
        SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let (events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
    let WorkerEvent::Compacted {
        notice, summary, ..
    } = &events[compactions(&events)[0]]
    else {
        unreachable!()
    };
    assert!(
        matches!(notice.summary_source, SummarySource::Runtime { .. }),
        "{notice:?}"
    );
    assert_eq!(summary.goal, INSTRUCTION);
    assert_eq!(
        summary.open_items,
        vec![
            "write a.py: no change to it is recorded yet".to_owned(),
            "write b.py: no change to it is recorded yet".to_owned(),
        ]
    );
    assert_eq!(notice.items_remaining, 2);
    // The model continues from it.
    let after = provider
        .requests()
        .into_iter()
        .find(|r| compacted(r) && !is_summary(r))
        .expect("a request after the compaction");
    let text = text_of(&after);
    assert!(text.contains("write a.py: no change"), "{text}");
}

/// `bad`, then a user's compaction, then `bad` again, then an answer. Returns
/// how many notices of `kind` the worker sent and how it ended.
async fn twice_around_a_compaction(
    bad: fn() -> Round,
    kind: kgraf_runtime::NoticeKind,
) -> (usize, WorkerResult) {
    let dir = tempfile::tempdir().expect("dir");
    let injector: Arc<std::sync::OnceLock<kgraf_runtime::Injector>> =
        Arc::new(std::sync::OnceLock::new());
    let after = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (inj, count) = (injector.clone(), after.clone());
    let provider = Fake::new(move |request| {
        if is_summary(request) {
            return answer(SUMMARY);
        }
        if compacted(request) {
            return match count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => bad(),
                _ => answer("done"),
            };
        }
        match rounds(request) {
            0 => tool(
                "a",
                "write_file",
                json!({"path": "a.txt", "content": "x"}),
                1_000,
            ),
            _ => {
                // The user asks for a compaction while this turn comes back.
                inj.get()
                    .expect("set")
                    .request_compaction()
                    .expect("accepted");
                bad()
            }
        }
    });
    let env = env_with(provider, false);
    let handle = spawn_worker(env, brief(dir.path()), SpawnOptions::default())
        .await
        .expect("spawn");
    assert!(injector.set(handle.injector()).is_ok());
    let (events, result) = drain(handle).await;
    assert_eq!(compactions(&events).len(), 1);
    (notices(&events, kind), result)
}

/// An empty turn, then a compaction, then an empty turn: the continuation
/// notice of the first is not in the remainder, so the second gets its own
/// notice instead of failing as "twice in a row".
#[tokio::test]
async fn an_empty_turn_after_a_compaction_gets_its_own_continuation() {
    let (sent, result) =
        twice_around_a_compaction(|| answer(""), kgraf_runtime::NoticeKind::EmptyTurn).await;
    assert_eq!(sent, 2);
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
}

/// The same for an output-ceiling stop: the cut-off turn it continues from is
/// not in the remainder.
#[tokio::test]
async fn an_output_ceiling_stop_after_a_compaction_gets_its_own_continuation() {
    let ceiling = || Round {
        stop: StopReason::MaxTokens,
        ..answer("*** Begin Patch\n*** Update File: a.txt")
    };
    let (sent, result) =
        twice_around_a_compaction(ceiling, kgraf_runtime::NoticeKind::OutputLimit).await;
    assert_eq!(sent, 2);
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");
}

/// Decision 0098, on the wire: a worker's rounds carry the level
/// `worker_round` resolves to and its compaction summary the level of
/// `worker_compaction`, each as the Chat Completions encoder writes it, and
/// not the universal level set beside them. Each ledger row records its kind.
#[tokio::test]
async fn a_workers_rounds_and_its_summary_send_their_own_reasoning_levels() {
    use kgraf_provider::{ReasoningEffort, ReasoningLevels, ReasoningProcess, ReasoningScope};
    let dir = tempfile::tempdir().expect("dir");
    let provider = Fake::new(growing_then_done(SUMMARY));
    let mut env = Arc::try_unwrap(env_with(provider.clone(), true))
        .ok()
        .expect("one owner");
    env.settings.reasoning = ReasoningLevels::new()
        .with(ReasoningScope::All, ReasoningEffort::Medium.into())
        .with(
            ReasoningScope::Process(ReasoningProcess::WorkerRound),
            ReasoningEffort::High.into(),
        )
        .with(
            ReasoningScope::Process(ReasoningProcess::WorkerCompaction),
            ReasoningEffort::Off.into(),
        );
    let env = Arc::new(env);
    let handle = spawn_worker(env.clone(), brief(dir.path()), compact_options())
        .await
        .expect("spawn");
    let (_events, result) = drain(handle).await;
    assert_eq!(result.outcome, WorkerOutcome::Completed, "{result:?}");

    let wire = kgraf_provider_openai::ChatCompletionsAdapter::new(
        kgraf_provider::ProviderConfig::local(
            "http://127.0.0.1:9/v1",
            vec![MODEL.into()],
            Protocol::Inclusive,
        ),
        kgraf_provider_openai::UsageDialect::Details,
    )
    .expect("adapter");
    let requests = provider.requests();
    let summaries = requests.iter().filter(|r| is_summary(r)).count();
    assert_eq!(summaries, 1, "one summary call");
    assert!(requests.len() > 2, "rounds before and after it");
    for request in &requests {
        let body = wire.to_wire(request);
        let want = if is_summary(request) { "none" } else { "high" };
        assert_eq!(
            body["reasoning_effort"],
            json!(want),
            "{}",
            text_of(request)
        );
    }
    let kinds: Vec<Option<String>> = result
        .calls
        .iter()
        .map(|c| env.ledger.call(c).expect("read").expect("row").trace.kind)
        .collect();
    assert!(kinds.iter().all(|k| matches!(
        k.as_deref(),
        Some("worker_round") | Some(COMPACTION_CALL_KIND)
    )));
}
