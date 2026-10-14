//! The prompt-cache layout of the delta call: golden requests with the
//! option off, the plan block's byte stability and order with it on.
//!
//! The three golden fixtures under `tests/fixtures/cache-layout/` were written
//! by this module's `golden_*` tests against the code from before the option
//! existed, with `KGRAF_WRITE_CACHE_GOLDEN` set to the fixtures' directory.

use super::tests::{change, record, spec};
use super::*;
use crate::plan::{
    Decomposer, DeltaRequest, GroundRequest, Grounder, ModelCaller, ModelDecomposer, ModelGrounder,
    TriageRequest,
};
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::{CallToken, Content, Provider, ProviderError, Request, Response, StopReason};
use std::sync::{Arc, Mutex};

/// Records each request whole (the debug form) and answers by call kind.
pub(super) struct Capture {
    pub(super) requests: Mutex<Vec<Request>>,
    pub(super) replies: Mutex<Vec<String>>,
}

impl Capture {
    pub(super) fn new(replies: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.iter().rev().map(|s| (*s).to_owned()).collect()),
        })
    }

    pub(super) fn seen(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
    }
}

#[async_trait::async_trait]
impl Provider for Capture {
    fn adapter(&self) -> &'static str {
        "capture"
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
        let reply = self
            .replies
            .lock()
            .expect("lock")
            .pop()
            .expect("a scripted reply");
        let usage = TokenUsage {
            uncached_input: 200,
            text_output: 50,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        // A reply marked `CUT|` ends at the output limit.
        let (reply, stop_reason) = match reply.strip_prefix("CUT|") {
            Some(cut) => (cut.to_owned(), StopReason::MaxTokens),
            None => (reply, StopReason::EndTurn),
        };
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![Content::text(reply)],
            stop_reason,
            usage,
            request_id: None,
        })
    }
}

pub(super) fn caller(provider: Arc<dyn Provider>) -> ModelCaller {
    ModelCaller {
        reasoning: Default::default(),
        provider,
        ledger: Arc::new(kgraf_provider::Ledger::open_in_memory().expect("ledger")),
        model: "m".into(),
        max_output_tokens: 4_096,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    }
}

const DELTA_REPLY: &str = r#"{"kind":"delta","add":[{"id":"n1","instruction":"make n1","acceptance":"src/n1.rs exists","writes":[{"path":"src/n1.rs"}],"depends_on":[],"tools":["write_file"]}]}"#;

/// Two verified and two open contracts, inserted out of id order.
pub(super) fn shuffled_record() -> RunStateRecord {
    let v2 = spec("v2", "src/v2.rs", &[]);
    let v1 = spec("v1", "src/v1.rs", &["v2"]);
    let o3 = spec("o3", "src/o3.rs", &["v1"]);
    let o1 = spec("o1", "src/o1.rs", &[]);
    record(&[v2, v1], &[o3, o1])
}

fn golden(name: &str, got: &str) {
    if let Ok(dir) = std::env::var("KGRAF_WRITE_CACHE_GOLDEN") {
        std::fs::write(std::path::Path::new(&dir).join(name), got).expect("write the fixture");
        return;
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cache-layout")
        .join(name);
    let want = std::fs::read_to_string(&path).expect("fixture");
    assert!(
        got == want,
        "the request changed with the cache layout off:\n--- before\n{want}\n--- now\n{got}"
    );
}

#[tokio::test]
async fn golden_replan_delta_request_with_the_layout_off() {
    let provider = Capture::new(&[DELTA_REPLY]);
    let r = shuffled_record();
    let c = change(&r);
    let request = DeltaRequest {
        prompt: delta_prompt(&r, &c, "Project context.", false),
        cap: output_cap(&r, &c),
        own_checks: false,
        cache_layout: false,
        mark_plan: false,
    };
    ModelDecomposer(caller(provider.clone()))
        .replan_delta(&request)
        .await
        .expect("answer");
    golden("replan-delta.txt", &format!("{:#?}", provider.seen()[0]));
}

#[tokio::test]
async fn golden_triage_request() {
    let provider = Capture::new(&[r#"{"size":"one","writes":["src/a.rs"]}"#]);
    ModelDecomposer(caller(provider.clone()))
        .triage(&TriageRequest {
            request: "make a".into(),
            named: vec!["src/a.rs".into()],
            known_files: vec!["src/a.rs".into(), "src/b.rs".into()],
            deadline: None,
        })
        .await
        .expect("answer");
    golden("triage.txt", &format!("{:#?}", provider.seen()[0]));
}

#[tokio::test]
async fn golden_reground_request() {
    let provider = Capture::new(&[r#"{"serves":true,"reason":"on track"}"#]);
    ModelGrounder(caller(provider.clone()))
        .ground(&GroundRequest {
            original_instruction: "make a then b".into(),
            next_contract: "b".into(),
            next_instruction: "make b".into(),
            verified: vec!["a".into()],
            notes: vec![],
        })
        .await
        .expect("answer");
    golden("reground.txt", &format!("{:#?}", provider.seen()[0]));
}

/// The contract ids of the plan block's lines, in order.
fn plan_ids(prompt: &DeltaPrompt) -> Vec<String> {
    plan_lines(prompt)
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("a JSON line");
            v["id"].as_str().expect("an id").to_owned()
        })
        .collect()
}

/// The plan block's lines, between the tags.
fn plan_lines(prompt: &DeltaPrompt) -> Vec<String> {
    prompt
        .plan
        .lines()
        .skip_while(|l| *l != "<plan>")
        .skip(1)
        .take_while(|l| *l != "</plan>")
        .map(str::to_owned)
        .collect()
}

fn sorted_record() -> RunStateRecord {
    let v1 = spec("v1", "src/v1.rs", &["v2"]);
    let v2 = spec("v2", "src/v2.rs", &[]);
    let o1 = spec("o1", "src/o1.rs", &[]);
    let o3 = spec("o3", "src/o3.rs", &["v1"]);
    record(&[v1, v2], &[o1, o3])
}

/// The plan block with the layout on is one text whatever order the record
/// was built in, and lists every contract by id; without it, the order is
/// the record's, open contracts first, as before.
#[test]
fn the_plan_block_lists_contracts_by_id_whatever_the_insertion_order() {
    let shuffled = shuffled_record();
    let sorted = sorted_record();
    let (a, b) = (
        delta_prompt(&shuffled, &change(&shuffled), "ctx", true),
        delta_prompt(&sorted, &change(&sorted), "ctx", true),
    );
    assert_eq!(a.plan, b.plan);
    assert_eq!(plan_ids(&a), ["o1", "o3", "v1", "v2"]);
    // A record that kept no spec for an open contract rebuilds it from its
    // open record: the same line whatever the order.
    let (mut x, mut y) = (shuffled_record(), sorted_record());
    x.plan_specs.remove("o1");
    y.plan_specs.remove("o1");
    assert_eq!(
        delta_prompt(&x, &change(&x), "ctx", true).plan,
        delta_prompt(&y, &change(&y), "ctx", true).plan
    );
    // Off: open contracts first, then closed ones, in record order.
    let off = delta_prompt(&shuffled, &change(&shuffled), "ctx", false);
    assert_eq!(plan_ids(&off), ["o3", "o1", "v2", "v1"]);
}

/// A contract that closes rewrites its own line and no other with the layout
/// on; with it off, the move from the open lines to the closed ones shifts
/// every line between.
#[test]
fn a_contract_closing_changes_only_its_own_lines() {
    let (v1, v2) = (
        spec("v1", "src/v1.rs", &["v2"]),
        spec("v2", "src/v2.rs", &[]),
    );
    let (o1, o3) = (
        spec("o1", "src/o1.rs", &[]),
        spec("o3", "src/o3.rs", &["v1"]),
    );
    let open = record(&[v2.clone(), v1.clone()], &[o3.clone(), o1.clone()]);
    let closed = record(
        &[v2.clone(), v1.clone(), o1.clone()],
        std::slice::from_ref(&o3),
    );
    // The closed contracts' order in the record is no part of a line.
    let reversed = record(
        &[o1.clone(), v1.clone(), v2.clone()],
        std::slice::from_ref(&o3),
    );
    let differing = |by_id: bool| -> Vec<usize> {
        let before = plan_lines(&delta_prompt(&open, &change(&open), "", by_id));
        let after = plan_lines(&delta_prompt(&closed, &change(&closed), "", by_id));
        assert_eq!(before.len(), after.len());
        (0..before.len())
            .filter(|i| before[*i] != after[*i])
            .collect()
    };
    assert_eq!(differing(true), vec![0], "only o1's line, the first by id");
    assert!(
        differing(false).len() > 1,
        "without the layout the lines shift"
    );
    let after = delta_prompt(&closed, &change(&closed), "", true);
    assert_eq!(
        after.plan,
        delta_prompt(&reversed, &change(&reversed), "", true).plan
    );
    let line: serde_json::Value = serde_json::from_str(&plan_lines(&after)[0]).expect("json");
    assert!(
        line.get("acceptance").is_none(),
        "a closed line is the short form: {line}"
    );
    // An amendment of an open contract's text rewrites its line alone.
    let mut amended = open.clone();
    amended.open_contracts[0].instruction = "do o3 differently".into();
    let before = plan_lines(&delta_prompt(&open, &change(&open), "", true));
    let after = plan_lines(&delta_prompt(&amended, &change(&amended), "", true));
    let differing: Vec<usize> = (0..before.len())
        .filter(|i| before[*i] != after[*i])
        .collect();
    assert_eq!(differing.len(), 1, "{differing:?}");
    assert!(after[differing[0]].contains("do o3 differently"));
}

/// The plan block is a cache block only when asked: one unmarked block
/// without, a marked plan block and the rest with, and the same text either
/// way.
#[test]
fn the_plan_segment_is_a_cache_block_only_when_asked() {
    let r = shuffled_record();
    let prompt = delta_prompt(&r, &change(&r), "ctx", true);
    let off = prompt.content(false);
    assert_eq!(off, vec![Content::text(prompt.text())]);
    let on = prompt.content(true);
    assert_eq!(on.len(), 2);
    assert!(matches!(
        &on[0],
        Content::Text {
            cache_breakpoint: true,
            text,
            ..
        } if *text == prompt.plan
    ));
    assert!(matches!(
        &on[1],
        Content::Text {
            cache_breakpoint: false,
            ..
        }
    ));
    let joined: String = on.iter().filter_map(Content::as_text).collect();
    assert_eq!(joined, prompt.text());
}

fn request_of(r: &RunStateRecord, c: &RecordedChange, layout: bool, mark: bool) -> DeltaRequest {
    DeltaRequest {
        prompt: delta_prompt(r, c, "Project context.", layout),
        cap: output_cap(r, c),
        own_checks: false,
        cache_layout: layout,
        mark_plan: mark,
    }
}

/// With the layout off the request carries no mark and no key (the golden
/// fixture pins the whole request); on, it carries the key, and the mark
/// where the lifetime gate allowed one.
#[tokio::test]
async fn the_request_carries_the_mark_and_the_run_key_only_with_the_layout_on() {
    let r = shuffled_record();
    let c = change(&r);
    let provider = Capture::new(&[DELTA_REPLY, DELTA_REPLY, DELTA_REPLY]);
    let decomposer = ModelDecomposer(caller(provider.clone()));
    for (layout, mark) in [(false, false), (true, true), (true, false)] {
        decomposer
            .replan_delta(&request_of(&r, &c, layout, mark))
            .await
            .expect("answer");
    }
    let seen = provider.seen();
    let marked = |q: &Request| {
        q.messages[0].content.iter().any(|b| {
            matches!(
                b,
                Content::Text {
                    cache_breakpoint: true,
                    ..
                }
            )
        })
    };
    assert!(!marked(&seen[0]) && seen[0].cache_key.is_none());
    // The adapter may drop only marks the layout placed.
    assert_eq!(
        seen.iter().map(|q| q.layout_marks).collect::<Vec<_>>(),
        [false, true, false]
    );
    assert_eq!(seen[0].messages[0].content.len(), 1);
    assert!(marked(&seen[1]));
    assert_eq!(seen[1].cache_key.as_deref(), Some("run:r1"));
    assert!(!marked(&seen[2]), "the lifetime gate said no mark");
    assert_eq!(seen[2].cache_key.as_deref(), Some("run:r1"));
}

/// Two calls on an unchanged record, a re-ask after a refused answer and the
/// retry with the cap doubled all send the same bytes through the plan block.
#[tokio::test]
async fn the_plan_block_is_byte_identical_across_calls_re_asks_and_cap_retries() {
    let r = shuffled_record();
    let c = change(&r);
    let provider = Capture::new(&[
        DELTA_REPLY,
        DELTA_REPLY,
        DELTA_REPLY,
        "CUT|{\"kind\":\"delta\",\"add\":[{\"id\":",
        DELTA_REPLY,
    ]);
    let decomposer = ModelDecomposer(caller(provider.clone()));
    decomposer
        .replan_delta(&request_of(&r, &c, true, true))
        .await
        .expect("first");
    decomposer
        .replan_delta(&request_of(&r, &c, true, true))
        .await
        .expect("second, the same record");
    let mut refused = c.clone();
    refused.refusal = Some("an id was reused".into());
    decomposer
        .replan_delta(&request_of(&r, &refused, true, true))
        .await
        .expect("re-ask after a refused answer");
    decomposer
        .replan_delta(&request_of(&r, &c, true, true))
        .await
        .expect("answered after the cut");
    let seen = provider.seen();
    assert_eq!(seen.len(), 5, "the cut answer was asked once more");
    let plan = |q: &Request| q.messages[0].content[0].clone();
    for q in &seen[1..] {
        assert_eq!(plan(q), plan(&seen[0]));
    }
    assert_eq!(seen[0].messages, seen[1].messages, "an unchanged record");
    assert_ne!(seen[2].messages, seen[0].messages, "the refusal is quoted");
    assert!(
        seen[2].messages[0].content[1]
            .as_text()
            .is_some_and(|t| t.contains("<answer_refused>")),
        "after the plan block, not in it"
    );
    assert_eq!(
        seen[4].max_output_tokens,
        seen[3].max_output_tokens * 2,
        "the cap doubled"
    );
    assert_eq!(seen[4].messages, seen[3].messages, "the retry's bytes");
}

/// The lifetime gate, on stub times: no mark where the route takes none, a
/// mark with no earlier call or one inside the lifetime, none after it.
#[test]
fn the_plan_mark_is_gated_on_the_cache_lifetime() {
    use std::time::Duration;
    let ttl = Some(Duration::from_secs(300));
    let now = 1_000_000;
    assert!(!plan_mark_due(None, None, now), "a route with no marks");
    assert!(!plan_mark_due(None, Some(now), now));
    assert!(plan_mark_due(ttl, None, now), "no earlier call to age");
    assert!(plan_mark_due(ttl, Some(now - 299_000), now));
    assert!(plan_mark_due(ttl, Some(now - 300_000), now), "at the limit");
    assert!(!plan_mark_due(ttl, Some(now - 300_001), now));
    assert!(!plan_mark_due(ttl, Some(now - 3_600_000), now));
    // The previous call is read from the record: the newest earlier change
    // that made a call, not the one being asked, and not one that made none.
    let mut r = shuffled_record();
    let mut at = |n: u32, ms: i64, calls: &[&str]| {
        let mut c = RecordedChange::received(n, ChangeSource::Change, "x", None, ms, 1);
        c.calls = calls.iter().map(|s| (*s).to_owned()).collect();
        r.changes.push(c);
    };
    at(1, 100, &["a"]);
    at(2, 200, &["b"]);
    at(3, 300, &[]);
    at(4, 400, &["c"]);
    assert_eq!(previous_delta_ms(&r, 4, None), Some(200));
    assert_eq!(previous_delta_ms(&r, 5, None), Some(400));
    assert_eq!(previous_delta_ms(&r, 4, Some(250)), Some(250));
    assert_eq!(previous_delta_ms(&shuffled_record(), 1, None), None);
}

/// The report counts the run's delta calls and re-asks, the figures a run
/// with the cache layout on is compared with one off by.
#[test]
fn the_counts_hold_delta_calls_and_re_asks() {
    let mut r = shuffled_record();
    let mut one = RecordedChange::received(1, ChangeSource::Change, "x", None, 1, 1);
    one.calls = vec!["a".into(), "b".into()];
    one.reasks = 1;
    let mut two = RecordedChange::received(2, ChangeSource::Change, "y", None, 2, 1);
    two.calls = vec!["c".into(), "d".into(), "e".into()];
    two.reasks = 1;
    two.conflict_reasks = 1;
    r.changes = vec![one, two];
    let k = counts(&r);
    assert_eq!((k.delta_calls, k.delta_reasks), (5, 3));
}

/// A contract's line carries the text the model is shown (`model_copy` of
/// the stored instruction), whatever order the record was built in: the
/// redaction is a function of that contract's text alone.
#[test]
fn a_redacted_instruction_gives_the_same_line_in_any_insertion_order() {
    let raw = format!("push with token ghp_{} when done", "a".repeat(36));
    let stored = kgraf_secrets::intake(&raw);
    let shown = kgraf_secrets::model_copy(&stored).into_owned();
    assert_ne!(stored, shown, "the model's copy is not the stored one");
    let secret = spec("o2", "src/o2.rs", &[]);
    let mut secret = secret;
    secret.instruction = stored.clone();
    let others = [spec("o1", "src/o1.rs", &[]), spec("o3", "src/o3.rs", &[])];
    let forward = record(&[], &[others[0].clone(), secret.clone(), others[1].clone()]);
    let backward = record(&[], &[others[1].clone(), secret.clone(), others[0].clone()]);
    let a = delta_prompt(&forward, &change(&forward), "", true);
    let b = delta_prompt(&backward, &change(&backward), "", true);
    assert_eq!(a.plan, b.plan);
    assert_eq!(plan_ids(&a), ["o1", "o2", "o3"]);
    let line: serde_json::Value = serde_json::from_str(&plan_lines(&a)[1]).expect("json");
    assert_eq!(line["instruction"], shown.as_str());
    // The stored text, with its redaction marker, never reaches the plan block.
    assert!(
        !a.plan.contains(&stored),
        "the model's copy is what it is shown"
    );
}
