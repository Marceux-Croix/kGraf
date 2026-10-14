//! Answers the model ended itself before their JSON object closed: closed
//! when only closing brackets are missing, asked for once more otherwise,
//! never for a cut at the output limit.
//!
//! The fixtures are answers a local model gave the planner (normal stop,
//! output limit 64,000 tokens, far from reached), copied whole:
//!
//! - `interfaces-final-brace-missing-a.txt`, `-b.txt`: interface answers
//!   that end `}]` with the object's final `}` missing.
//! - `interfaces-stops-mid-word.txt`: an interface answer that stops inside a
//!   file's content, mid-word (`impl TaxRule for TaxU`).
//! - `decompose-stops-mid-word.txt`: a contracts answer in a code fence that
//!   stops inside a slice name (`"slices": ["Ship`).
//! - `reground-stops-mid-string.txt`: a re-grounding verdict that stops
//!   inside its `reason` string.

use super::*;
use crate::answer_json::{Closing, closing};
use kgraf_provider::{AnswerRecovery, AnswerRecoveryAction, StopReason};

const BRACE_MISSING_A: &str =
    include_str!("../tests/fixtures/incomplete-answers/interfaces-final-brace-missing-a.txt");
const BRACE_MISSING_B: &str =
    include_str!("../tests/fixtures/incomplete-answers/interfaces-final-brace-missing-b.txt");
const INTERFACES_MID_WORD: &str =
    include_str!("../tests/fixtures/incomplete-answers/interfaces-stops-mid-word.txt");
const DECOMPOSE_MID_WORD: &str =
    include_str!("../tests/fixtures/incomplete-answers/decompose-stops-mid-word.txt");
const REGROUND_MID_STRING: &str =
    include_str!("../tests/fixtures/incomplete-answers/reground-stops-mid-string.txt");

const WHOLE_PLAN: &str = r#"{"contracts":[{"id":"c1","instruction":"make a","acceptance":"tests pass","writes":[{"path":"src/a.rs"}]}]}"#;

/// A contracts answer that lacks only its closers, and whose closed form is
/// still refused: the contract has no `acceptance`.
const CLOSABLE_BUT_REFUSED: &str =
    r#"{"contracts":[{"id":"c1","instruction":"make a","writes":[{"path":"src/a.rs"}]}]"#;

fn interfaces(text: &str) -> Result<(), PlanError> {
    crate::interface_plan::parse_interfaces(text).map(|_| ())
}

// ---------------------------------------------------------------- the rule

#[test]
fn recorded_answers_missing_only_their_final_brace_are_closed_and_parse() {
    for (name, text) in [("a", BRACE_MISSING_A), ("b", BRACE_MISSING_B)] {
        assert!(interfaces(text).is_err(), "{name}: refused as written");
        let Closing::Closable {
            text: closed,
            appended,
        } = closing(text)
        else {
            panic!("{name}: not closable: {:?}", closing(text));
        };
        assert_eq!(appended, "}", "{name}");
        assert_eq!(closed, format!("{}}}", text.trim_end()), "{name}");
        let drafts = crate::interface_plan::parse_interfaces(&closed).expect("closed parses");
        assert!(!drafts.is_empty(), "{name}");
        match read_answer(text, true, &interfaces) {
            Reading::Parsed { appended, text, .. } => {
                assert_eq!(appended.as_deref(), Some("}"), "{name}");
                assert_eq!(text, closed, "{name}");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

/// The recorded verdict with its `reason` string closed and its brace
/// missing: the closed answer is a whole verdict.
#[test]
fn a_verdict_missing_only_its_brace_is_closed_and_parses() {
    let text = format!("{}\"", REGROUND_MID_STRING.trim_end());
    match read_answer(&text, true, &parse_grounding) {
        Reading::Parsed {
            value: (serves, reason),
            appended,
            ..
        } => {
            assert!(serves);
            assert!(reason.ends_with("`checkout::checkout_order`."), "{reason}");
            assert_eq!(appended.as_deref(), Some("}"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn recorded_answers_that_stop_inside_a_string_are_never_closed() {
    let interfaces_reading = read_answer(INTERFACES_MID_WORD, true, &interfaces);
    let decompose_reading = read_answer(DECOMPOSE_MID_WORD, true, &parse_plan);
    let reground_reading = read_answer(REGROUND_MID_STRING, true, &parse_grounding);
    for (name, text) in [
        ("interfaces", INTERFACES_MID_WORD),
        ("decompose", DECOMPOSE_MID_WORD),
        ("reground", REGROUND_MID_STRING),
    ] {
        assert_eq!(
            closing(text),
            Closing::Unclosable("the answer stops inside a string"),
            "{name}"
        );
    }
    for (name, reading) in [
        ("interfaces", format!("{interfaces_reading:?}")),
        ("decompose", format!("{decompose_reading:?}")),
        ("reground", format!("{reground_reading:?}")),
    ] {
        assert!(reading.starts_with("Incomplete"), "{name}: {reading}");
        assert!(reading.contains("inside a string"), "{name}: {reading}");
    }
}

#[test]
fn only_a_stop_right_after_a_complete_value_is_closable() {
    let closable = [
        (r#"{"a":[1,2]"#, "}"),
        (r#"{"a":{"b":"c""#, "}}"),
        (r#"{"a":true"#, "}"),
        (r#"{"a":[null"#, "]}"),
        ("```json\n{\"a\":[1]\n```\n", "}"),
        ("<think>{\"draft\":</think>\n{\"a\":\"b\"", "}"),
    ];
    for (text, want) in closable {
        match closing(text) {
            Closing::Closable {
                text: closed,
                appended,
            } => {
                assert_eq!(appended, want, "{text}");
                let v = json_object(&closed).expect("closed parses");
                assert!(v.get("a").is_some(), "{text}: {v}");
            }
            other => panic!("{text}: {other:?}"),
        }
    }
    let unclosable = [
        (r#"{"a":[1,"#, "comma"),
        (r#"{"a""#, "key"),
        (r#"{"a":"#, "key"),
        (r#"{"a":12"#, "number"),
        (r#"{"a":tr"#, "number or a word"),
        (r#"{"a":["#, "opening bracket"),
    ];
    for (text, why) in unclosable {
        match closing(text) {
            Closing::Unclosable(reason) => assert!(reason.contains(why), "{text}: {reason}"),
            other => panic!("{text}: {other:?}"),
        }
    }
    // An error before the end of the text is the answer's own mistake, not
    // an early stop: a mismatched bracket, a stray character.
    for text in [
        WHOLE_PLAN,
        "no object here",
        r#"{"a":{"b":[1}"#,
        r#"{"a":"b"`"#,
    ] {
        assert_eq!(closing(text), Closing::NotStoppedEarly, "{text}");
    }
}

/// A recorded contracts answer the model finished (it ends with its closing
/// brace and code fence) with one string left open in the middle: the open
/// string makes its last object look unclosed, but the parser stops at the
/// line break inside it, far from the end. It is refused with that reason,
/// never sent again as if it had stopped early.
#[test]
fn an_answer_with_a_string_left_open_mid_way_did_not_stop_early() {
    let text = include_str!("../tests/fixtures/planner-answers/d6-replan-unclosed-string.txt");
    assert_eq!(closing(text), Closing::NotStoppedEarly);
    let reading = read_answer(text, true, &parse_plan);
    let Reading::Refused(error) = reading else {
        panic!("{reading:?}");
    };
    assert!(error.to_string().contains("probably not closed"), "{error}");
}

/// An answer cut at the output limit is never closed, even when only its
/// closers are missing.
#[test]
fn an_answer_not_ended_by_the_model_is_never_closed() {
    let reading = read_answer(BRACE_MISSING_A, false, &interfaces);
    assert!(matches!(reading, Reading::Refused(_)), "{reading:?}");
}

#[test]
fn a_closed_answer_refused_by_its_parser_is_incomplete_not_parsed() {
    let reading = read_answer(CLOSABLE_BUT_REFUSED, true, &parse_plan);
    let Reading::Incomplete { why, error } = reading else {
        panic!("{reading:?}");
    };
    assert!(
        why.contains("closing it with `}` gives an answer that is still refused"),
        "{why}"
    );
    assert!(why.contains("acceptance"), "{why}");
    assert!(error.is_malformed_answer(), "{error}");
}

// ------------------------------------------------------------ the call path

/// Answers each call with the next scripted text and stop reason and keeps
/// each request's user text.
struct Script {
    replies: std::sync::Mutex<std::collections::VecDeque<(String, StopReason)>>,
    prompts: std::sync::Mutex<Vec<String>>,
}

impl Script {
    fn new(replies: &[(&str, StopReason)]) -> Arc<Self> {
        Arc::new(Self {
            replies: std::sync::Mutex::new(
                replies.iter().map(|(t, s)| ((*t).to_owned(), *s)).collect(),
            ),
            prompts: std::sync::Mutex::new(Vec::new()),
        })
    }
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("lock").clone()
    }
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
        request: &Request,
        token: &kgraf_provider::CallToken,
    ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
        let prompt = request
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(kgraf_provider::Content::as_text)
            .collect::<Vec<_>>()
            .join("\n");
        self.prompts.lock().expect("lock").push(prompt);
        let (text, stop_reason) = self
            .replies
            .lock()
            .expect("lock")
            .pop_front()
            .expect("a call past the script");
        let usage = kgraf_core::TokenUsage {
            uncached_input: 10,
            text_output: 10,
            ..Default::default()
        };
        token.settle(&usage, None)?;
        Ok(kgraf_provider::Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![kgraf_provider::Content::text(text)],
            stop_reason,
            usage,
            request_id: None,
        })
    }
}

fn caller(provider: Arc<Script>) -> ModelCaller {
    ModelCaller {
        reasoning: Default::default(),
        provider,
        ledger: Arc::new(Ledger::open_in_memory().expect("ledger")),
        model: "m".into(),
        max_output_tokens: 64_000,
        agent: "orchestrator:r".into(),
        task: "r".into(),
    }
}

fn plan_request() -> PlanRequest {
    PlanRequest {
        instruction: "make a".into(),
        split_reason: None,
        notes: vec![],
        known_files: vec![],
        routes: vec![],
        model_routes: vec![],
        model_calls: None,
        tools: vec![],
        templates: vec![],
        interfaces: false,
        project_context: String::new(),
        own_checks: false,
    }
}

fn interface_request() -> crate::interface_plan::InterfaceRequest {
    crate::interface_plan::InterfaceRequest {
        context: plan_request(),
        contracts: vec![],
        outer: vec![],
        consumers: vec![],
        accepted: vec![],
        supplied: vec![],
        output_cap: None,
        previous: None,
    }
}

fn recoveries(ledger: &Ledger) -> Vec<AnswerRecovery> {
    ledger.take_answer_recoveries("orchestrator:r").0
}

const END: StopReason = StopReason::EndTurn;

#[tokio::test]
async fn an_interface_answer_missing_its_final_brace_is_closed_on_the_same_call() {
    for text in [BRACE_MISSING_A, BRACE_MISSING_B] {
        let script = Script::new(&[(text, END)]);
        let d = ModelDecomposer(caller(script.clone()));
        let reply = d.interfaces(&interface_request()).await.expect("answered");
        assert_eq!(reply.calls.len(), 1, "no call added");
        assert_eq!(reply.text, format!("{}}}", text.trim_end()));
        assert_eq!(script.prompts().len(), 1);
        let kept = recoveries(&d.0.ledger);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].call, reply.calls[0], "names the call it closed");
        assert_eq!(kept[0].what, "interfaces");
        assert_eq!(
            kept[0].action,
            AnswerRecoveryAction::Repaired {
                appended: "}".into()
            }
        );
    }
}

#[tokio::test]
async fn an_interface_answer_stopped_mid_word_is_asked_for_once_more() {
    let script = Script::new(&[(INTERFACES_MID_WORD, END), (BRACE_MISSING_B, END)]);
    let d = ModelDecomposer(caller(script.clone()));
    let reply = d.interfaces(&interface_request()).await.expect("answered");
    assert_eq!(reply.calls.len(), 2, "both calls are the ask's");
    let prompts = script.prompts();
    assert!(!prompts[0].contains(INCOMPLETE_ANSWER_NOTE));
    assert_eq!(
        prompts[1],
        format!("{}\n\n{INCOMPLETE_ANSWER_NOTE}", prompts[0]),
        "the same request with one sentence added at its end"
    );
    let kept = recoveries(&d.0.ledger);
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert_eq!(kept[0].call, reply.calls[0]);
    let AnswerRecoveryAction::Reasked { reason } = &kept[0].action else {
        panic!("{kept:?}");
    };
    assert!(reason.contains("stops inside a string"), "{reason}");
    // The second answer lacked its brace: closed, on the second call.
    assert_eq!(kept[1].call, reply.calls[1]);
    assert!(matches!(
        kept[1].action,
        AnswerRecoveryAction::Repaired { .. }
    ));
}

#[tokio::test]
async fn a_decompose_answer_stopped_mid_word_is_asked_for_once_more() {
    let script = Script::new(&[(DECOMPOSE_MID_WORD, END), (WHOLE_PLAN, END)]);
    let d = ModelDecomposer(caller(script.clone()));
    let out = d.decompose(&plan_request()).await.expect("planned");
    assert_eq!(out.contracts.len(), 1);
    assert_eq!(out.calls.len(), 2, "every call is attributed to the plan");
    assert!(script.prompts()[1].ends_with(INCOMPLETE_ANSWER_NOTE));
    let kept = recoveries(&d.0.ledger);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].call, out.calls[0]);
    assert_eq!(kept[0].what, "decompose");
}

/// A second incomplete answer is the ask's refusal: one more call at most,
/// and the refusal is the one a planner's re-ask loop counts.
#[tokio::test]
async fn a_second_incomplete_answer_is_the_normal_refusal() {
    let script = Script::new(&[(DECOMPOSE_MID_WORD, END), (DECOMPOSE_MID_WORD, END)]);
    let d = ModelDecomposer(caller(script.clone()));
    let err = d.decompose(&plan_request()).await.expect_err("refused");
    assert_eq!(script.prompts().len(), 2, "one more call, never a loop");
    assert!(err.reaskable() && err.is_malformed_answer(), "{err}");
    assert!(err.to_string().contains("EOF while parsing"), "{err}");
    assert_eq!(err.calls().map(<[CallId]>::len), Some(2), "attributed");
    assert_eq!(recoveries(&d.0.ledger).len(), 1, "one re-ask recorded");
}

/// Through a JSON-asked call with its own bound (re-grounding): the free
/// re-ask is inside one ask, so the bound of asks is unchanged and a second
/// incomplete answer spends one of them.
#[tokio::test]
async fn a_regrounding_ask_counts_a_second_incomplete_answer_once() {
    let script = Script::new(&[
        (REGROUND_MID_STRING, END),
        (REGROUND_MID_STRING, END),
        (r#"{"serves":true,"reason":"on track"}"#, END),
    ]);
    let out = ModelGrounder(caller(script.clone()))
        .ground(&GroundRequest {
            original_instruction: "make a then b".into(),
            next_contract: "b".into(),
            next_instruction: "make b".into(),
            verified: vec!["a".into()],
            notes: vec![],
        })
        .await
        .expect("answered");
    assert!(out.serves);
    assert_eq!(out.retried_calls.len(), 2, "both calls of the refused ask");
    let prompts = script.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(
        prompts[1].ends_with(INCOMPLETE_ANSWER_NOTE),
        "{}",
        prompts[1]
    );
    assert!(!prompts[1].contains("<answer_refused>"));
    assert!(
        prompts[2].contains("<answer_refused>"),
        "the refusal counts"
    );
    assert!(!prompts[2].contains(INCOMPLETE_ANSWER_NOTE));
}

/// A cut at the output limit is unchanged: refused as cut off, one call,
/// never closed, never asked again.
#[tokio::test]
async fn a_length_stop_is_neither_closed_nor_asked_again() {
    let script = Script::new(&[(BRACE_MISSING_A, StopReason::MaxTokens)]);
    let d = ModelDecomposer(caller(script.clone()));
    let err = d
        .interfaces(&interface_request())
        .await
        .expect_err("cut off");
    assert!(
        matches!(&err, PlanError::Attributed { error, .. } if matches!(**error, PlanError::Truncated { .. })),
        "{err}"
    );
    assert!(!err.reaskable());
    assert_eq!(err.calls().map(<[CallId]>::len), Some(1));
    assert_eq!(script.prompts().len(), 1);
    assert!(recoveries(&d.0.ledger).is_empty());
}

/// A closed answer its parser still refuses is not taken: the request is
/// sent once more.
#[tokio::test]
async fn a_closed_answer_that_fails_its_parser_falls_through_to_the_re_ask() {
    let script = Script::new(&[(CLOSABLE_BUT_REFUSED, END), (WHOLE_PLAN, END)]);
    let d = ModelDecomposer(caller(script.clone()));
    let out = d.decompose(&plan_request()).await.expect("planned");
    assert_eq!(out.calls.len(), 2);
    assert_eq!(out.contracts[0].acceptance, "tests pass");
    let kept = recoveries(&d.0.ledger);
    assert_eq!(kept.len(), 1);
    let AnswerRecoveryAction::Reasked { reason } = &kept[0].action else {
        panic!("{kept:?}");
    };
    assert!(reason.contains("still refused"), "{reason}");
}

/// With model tools offered, the answer without tool calls is read the same
/// way: closed when it lacks only its closers, or the same round sent once
/// more, which is not a round of its own.
#[tokio::test]
async fn the_model_tool_path_closes_and_asks_once_more_the_same_way() {
    let ledger = Arc::new(Ledger::open_in_memory().expect("ledger"));
    let with_tools = |script: Arc<Script>| {
        ModelDecomposer(ModelCaller {
            ledger: ledger.clone(),
            ..caller(script)
        })
    };
    let request = || super::tests::tool_request(super::tests::rerank_calls(ledger.clone()));
    let closable = &WHOLE_PLAN[..WHOLE_PLAN.len() - 1];
    let script = Script::new(&[(closable, END)]);
    let out = with_tools(script.clone())
        .decompose(&request())
        .await
        .expect("planned");
    assert_eq!(out.calls.len(), 1, "closed on the same call");
    let kept = recoveries(&ledger);
    assert_eq!(kept.len(), 1);
    assert!(matches!(
        kept[0].action,
        AnswerRecoveryAction::Repaired { .. }
    ));

    let script = Script::new(&[(DECOMPOSE_MID_WORD, END), (WHOLE_PLAN, END)]);
    let out = with_tools(script.clone())
        .decompose(&request())
        .await
        .expect("planned");
    assert_eq!(out.calls.len(), 2);
    assert!(script.prompts()[1].ends_with(INCOMPLETE_ANSWER_NOTE));
    let kept = recoveries(&ledger);
    assert_eq!(kept.len(), 1);
    assert!(matches!(
        kept[0].action,
        AnswerRecoveryAction::Reasked { .. }
    ));
}

/// A whole answer is read as before: one call, nothing recorded.
#[tokio::test]
async fn a_whole_answer_is_read_unchanged() {
    let script = Script::new(&[(WHOLE_PLAN, END)]);
    let d = ModelDecomposer(caller(script.clone()));
    let out = d.decompose(&plan_request()).await.expect("planned");
    assert_eq!(out.calls.len(), 1);
    assert!(recoveries(&d.0.ledger).is_empty());
}
