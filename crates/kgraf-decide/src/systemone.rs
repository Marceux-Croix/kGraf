//! A client for the `/v1/systemone` wire format.
//!
//! Targeting a wire format rather than one project's Python API is the load-bearing
//! choice here. Several independent implementations already serve this endpoint, and
//! they occupy opposite ends of the tradeoff: a small model loaded in-process answers
//! in milliseconds on any CPU, while a much larger server reaches close to the hosted
//! original's accuracy and needs an NVIDIA card and a container runtime.
//!
//! kGraf's default has to be the first, because it ships to arbitrary consumer
//! machines on three platforms and kGraf runs no daemon. But a user who owns the
//! hardware should be able to trade the install for the accuracy, and speaking the
//! shared wire format is what makes that a configuration line instead of a port.
//!
//! # The format is the published one, not a plausible one
//!
//! The first version of this file was written from the *idea* of the endpoint and got
//! its shape wrong in five places at once: a state array of labelled blocks, a question
//! array, no `model` field, an answer array, and a `p_yes` probability. The real format
//! is a **state value**, a **question map keyed by name**, a **required `model`**, an
//! **answer map keyed by the same names**, and a noul probability in a field called
//! `noul`. Every call the old client could have made would have been rejected with a
//! validation error before a model saw it.
//!
//! That is the test-the-mechanism failure in its exact form, a mechanism asserted rather than exercised, and
//! it survived a full green gate because every test in the file mocked the format the
//! client already believed in. A mock is a restatement of an assumption, not
//! evidence about a protocol. The tests below still use a mock, because a unit test
//! must; what makes them worth anything now is a probe running the same client
//! against a real server.
//!
//! # Differences that are load-bearing
//!
//! **A noul carries no confidence.** The format publishes a probability and nothing
//! else for a yes/no question; only `choice` and `score` report a separate peakedness
//! figure. [`crate::Answer::Noul`] therefore holds an `Option`, and nothing here fills
//! it in from the probability.
//!
//! **Names key both directions.** Answers come back in a map, so a reordered batch is
//! a non-problem and a *duplicate* question id becomes the real hazard: it would drop a
//! question and answer a different one under its name. That is refused before anything
//! is sent.
//!
//! **A score is an expectation over level indices**, not a value in the caller's range.
//! The server returns something like 1.7 across three levels; this maps it back onto the
//! `min..=max` the caller asked in, which is why the level list is generated from that
//! range rather than passed through.

use crate::decider::{Acceleration, DecideError, Decider, verify_answers, verify_state_size};
use crate::decision::{Answer, AnswerSpace, Confidence, Question, State};
use kgraf_provider::CallToken;

/// This client's stable name.
pub const BACKEND: &str = "systemone-http";

/// The model name sent when the caller names none.
///
/// The field is **required** by the format, and every implementation surveyed accepts
/// this alias and routes it to whatever checkpoint it is serving. Omitting the field is
/// a validation error, which is how the first version of this client would have failed
/// on every call.
pub const DEFAULT_MODEL: &str = "jev-latest";

/// A `/v1/systemone` speaker reached over HTTP.
pub struct SystemOneClient {
    base_url: String,
    http: reqwest::Client,
    state_token_limit: u64,
    /// Sent as `model`. Required by the format.
    model: String,
    /// Sent as a bearer token when present. Absent by default: the implementations that
    /// serve this endpoint locally start with authentication off.
    api_key: Option<String>,
    /// What the server reports doing, or `Remote` when it does not say.
    acceleration: Acceleration,
}

impl SystemOneClient {
    /// A client against a server root, without the `/v1/systemone` suffix.
    ///
    /// No credential by default: every implementation of this endpoint surveyed is
    /// something the user runs themselves, on their own machine or their own network,
    /// with authentication off until they turn it on. [`Self::with_api_key`] is there for
    /// when they have.
    pub fn new(base_url: impl Into<String>, state_token_limit: u64) -> Result<Self, DecideError> {
        let http = reqwest::Client::builder()
            // Local inference on a CPU can be slow, and there is no rate limit to
            // protect, but a decision that takes a minute is a design error rather than
            // something to wait for: the whole premise is that these are cheap enough to
            // ask constantly.
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| DecideError::Unreachable {
                backend: BACKEND,
                reason: e.to_string(),
            })?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            http,
            state_token_limit,
            model: DEFAULT_MODEL.to_owned(),
            api_key: None,
            acceleration: Acceleration::Remote,
        })
    }

    /// Ask for a named checkpoint rather than the server's default alias.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Send a bearer token, for a server the user has put behind a key.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Record what the server says it is running on.
    pub fn with_acceleration(mut self, acceleration: Acceleration) -> Self {
        self.acceleration = acceleration;
        self
    }

    /// The model name this client asks for.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The level labels a score question is rendered as.
    ///
    /// The format takes an ordered list of levels and answers with an expectation over
    /// their indices. A caller's `min..=max` becomes that list here, and
    /// [`Self::score_from_wire`] maps the answer back, so a `Score` question means the
    /// same thing on every backend.
    fn score_levels(min: i32, max: i32) -> Vec<String> {
        (min..=max).map(|level| level.to_string()).collect()
    }

    /// Map an expectation over level indices back onto the caller's range.
    fn score_from_wire(min: i32, max: i32, wire: f64) -> i32 {
        if !wire.is_finite() {
            return min;
        }
        let index = wire.round();
        let span = (max - min) as f64;
        (min as f64 + index.clamp(0.0, span)) as i32
    }

    /// The request body for a batch.
    fn to_wire(
        &self,
        state: &State,
        questions: &[Question],
    ) -> Result<serde_json::Value, DecideError> {
        let mut wire_questions = serde_json::Map::new();
        for question in questions {
            let mut wire = serde_json::Map::new();
            match &question.space {
                AnswerSpace::Noul => {
                    wire.insert("type".into(), "noul".into());
                }
                AnswerSpace::Choice { options } => {
                    wire.insert("type".into(), "choice".into());
                    // Option name to description. kGraf's options are bare names, so
                    // every description is null rather than a restatement of the name.
                    let criteria: serde_json::Map<String, serde_json::Value> = options
                        .iter()
                        .map(|option| (option.clone(), serde_json::Value::Null))
                        .collect();
                    wire.insert("criteria".into(), serde_json::Value::Object(criteria));
                }
                AnswerSpace::Score { min, max } => {
                    wire.insert("type".into(), "score".into());
                    wire.insert(
                        "criteria".into(),
                        serde_json::json!(Self::score_levels(*min, *max)),
                    );
                }
            }
            wire.insert("instructions".into(), question.prompt.clone().into());
            if wire_questions
                .insert(question.id.clone(), serde_json::Value::Object(wire))
                .is_some()
            {
                return Err(DecideError::DuplicateQuestionId {
                    backend: BACKEND,
                    id: question.id.clone(),
                });
            }
        }
        Ok(serde_json::json!({
            // A plain string, which is the format's primary state shape. The label is
            // kGraf's own handle for the recorded decision and is deliberately not
            // rendered into the model's prompt: it names the state, it is not part of it.
            "state": state.body,
            "model": self.model,
            "questions": serde_json::Value::Object(wire_questions),
        }))
    }

    /// Decode one answer per question, by name.
    fn from_wire(
        questions: &[Question],
        body: &serde_json::Value,
    ) -> Result<Vec<Answer>, DecideError> {
        let rows = body
            .get("answers")
            .and_then(|a| a.as_object())
            .ok_or_else(|| DecideError::Decode {
                backend: BACKEND,
                reason: "reply has no `answers` object".to_owned(),
            })?;
        if rows.len() != questions.len() {
            return Err(DecideError::AnswerCountMismatch {
                backend: BACKEND,
                asked: questions.len(),
                returned: rows.len(),
            });
        }

        let mut out = Vec::with_capacity(questions.len());
        for question in questions {
            let row = rows
                .get(&question.id)
                .ok_or_else(|| DecideError::MissingAnswer {
                    backend: BACKEND,
                    question: question.id.clone(),
                })?;
            // Where the server names the shape it answered in, a disagreement is caught
            // here rather than presenting as a missing field.
            let declared = row.get("type").and_then(|t| t.as_str());
            let expected = match &question.space {
                AnswerSpace::Noul => "noul",
                AnswerSpace::Choice { .. } => "choice",
                AnswerSpace::Score { .. } => "score",
            };
            if let Some(declared) = declared
                && declared != expected
            {
                return Err(DecideError::OutsideAnswerSpace {
                    backend: BACKEND,
                    question: question.id.clone(),
                });
            }

            let answer = match &question.space {
                AnswerSpace::Noul => {
                    let p_yes = row.get("noul").and_then(|p| p.as_f64()).ok_or_else(|| {
                        DecideError::Decode {
                            backend: BACKEND,
                            reason: format!(
                                "noul answer for `{}` carries no `noul` probability",
                                question.id
                            ),
                        }
                    })?;
                    // No confidence, because the format publishes none for a noul. Not a
                    // zero, and not `|2p - 1|` dressed up as the model's own figure.
                    Answer::Noul {
                        p_yes,
                        confidence: row
                            .get("confidence")
                            .and_then(|c| c.as_f64())
                            .map(Confidence::new),
                    }
                }
                AnswerSpace::Choice { options } => {
                    let confidence = Confidence::new(
                        row.get("confidence")
                            .and_then(|c| c.as_f64())
                            .unwrap_or(0.0),
                    );
                    let chosen = row.get("choice").and_then(|c| c.as_str()).ok_or_else(|| {
                        DecideError::Decode {
                            backend: BACKEND,
                            reason: format!(
                                "choice answer for `{}` names no chosen option",
                                question.id
                            ),
                        }
                    })?;
                    let index = options
                        .iter()
                        .position(|option| option == chosen)
                        .ok_or_else(|| DecideError::OutsideAnswerSpace {
                            backend: BACKEND,
                            question: question.id.clone(),
                        })?;
                    // The distribution arrives keyed by option name. It is re-ordered
                    // into the caller's option order, because an index into a map's
                    // arrival order means nothing.
                    let probabilities = match row.get("probabilities").and_then(|p| p.as_object()) {
                        Some(map) => options
                            .iter()
                            .map(|option| map.get(option).and_then(|p| p.as_f64()).unwrap_or(0.0))
                            .collect(),
                        None => Vec::new(),
                    };
                    Answer::Choice {
                        index,
                        probabilities,
                        confidence,
                    }
                }
                AnswerSpace::Score { min, max } => {
                    let confidence = Confidence::new(
                        row.get("confidence")
                            .and_then(|c| c.as_f64())
                            .unwrap_or(0.0),
                    );
                    let wire = row.get("score").and_then(|v| v.as_f64()).ok_or_else(|| {
                        DecideError::Decode {
                            backend: BACKEND,
                            reason: format!("score answer for `{}` carries no value", question.id),
                        }
                    })?;
                    Answer::Score {
                        value: Self::score_from_wire(*min, *max, wire),
                        confidence,
                    }
                }
            };
            out.push(answer);
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Decider for SystemOneClient {
    fn backend(&self) -> &'static str {
        BACKEND
    }

    fn acceleration(&self) -> Acceleration {
        self.acceleration
    }

    fn state_token_limit(&self) -> u64 {
        self.state_token_limit
    }

    async fn decide(
        &self,
        state: &State,
        questions: &[Question],
        token: &CallToken,
    ) -> Result<Vec<Answer>, DecideError> {
        verify_state_size(BACKEND, state, self.state_token_limit)?;
        // A malformed batch never reaches the wire, and never opens a ledger row it
        // would then have to close.
        let body = match self.to_wire(state, questions) {
            Ok(body) => body,
            Err(error) => {
                token.fail_async().await?;
                return Err(error);
            }
        };
        let mut request = self
            .http
            .post(format!("{}/v1/systemone", self.base_url))
            .json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                // Nothing was produced, so nothing billed. A local decider costs no
                // money regardless, but the row still closes rather than dangling in the
                // unreconciled list a person has to work through.
                token.fail_async().await?;
                return Err(DecideError::Unreachable {
                    backend: BACKEND,
                    reason: e.to_string(),
                });
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            token.fail_async().await?;
            return Err(DecideError::Unreachable {
                backend: BACKEND,
                reason: format!("HTTP {status}: {text}"),
            });
        }
        let text = match response.text().await {
            Ok(text) => text,
            Err(e) => {
                token.fail_async().await?;
                return Err(DecideError::Unreachable {
                    backend: BACKEND,
                    reason: e.to_string(),
                });
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => {
                token.fail_async().await?;
                return Err(DecideError::Decode {
                    backend: BACKEND,
                    reason: e.to_string(),
                });
            }
        };
        let answers = match Self::from_wire(questions, &parsed) {
            Ok(answers) => answers,
            Err(error) => {
                token.fail_async().await?;
                return Err(error);
            }
        };
        if let Err(error) = verify_answers(BACKEND, questions, &answers) {
            token.fail_async().await?;
            return Err(error);
        }

        // Usage, where the server reports it. A local model's token count is real even
        // though its cost is zero, and recording it is what keeps the token-attribution rule literally true
        // rather than approximately true.
        let usage = parsed
            .get("usage")
            .and_then(|u| u.get("input_tokens"))
            .and_then(|t| t.as_u64())
            .map(|input| kgraf_core::TokenUsage {
                uncached_input: input,
                text_output: parsed
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0),
                raw: parsed.get("usage").cloned(),
                ..kgraf_core::TokenUsage::default()
            })
            .unwrap_or(kgraf_core::TokenUsage {
                // The state's estimate, clearly marked as locally synthesized by the
                // absent raw blob rather than passed off as the server's own figure.
                uncached_input: state.estimated_tokens(),
                ..kgraf_core::TokenUsage::default()
            });
        token.settle_async(&usage, None).await?;
        Ok(answers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::Protocol;
    use kgraf_provider::{Ledger, Settlement};
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn state() -> State {
        State::new("pair", "task A: rename the parser\ntask B: add a cache")
    }

    fn ledger_token(ledger: &Ledger) -> kgraf_provider::CallToken<'_> {
        ledger
            .begin("systemone-http", Protocol::Inclusive, "d", "a", "t")
            .expect("begin")
    }

    #[test]
    fn a_request_matches_the_published_schema_field_for_field() {
        // The assertion the first version of this file lacked. Not "does our client
        // round-trip its own mock", but "is this the document the server validates
        // against": a state value, a required model, and questions keyed by name.
        let client = SystemOneClient::new("http://localhost:8000", 8192).expect("client");
        let questions = vec![
            Question::noul("collides", "will these collide?"),
            Question::choice(
                "vendor",
                "which vendor",
                vec!["openai".into(), "anthropic".into()],
            )
            .expect("q"),
            Question::score("urgency", "how urgent", 1, 3).expect("q"),
        ];
        let wire = client.to_wire(&state(), &questions).expect("wire");

        assert_eq!(
            wire["state"],
            json!("task A: rename the parser\ntask B: add a cache")
        );
        assert_eq!(wire["model"], json!(DEFAULT_MODEL), "`model` is required");
        assert!(
            wire["state"].is_string(),
            "state is a value, not a block array"
        );
        assert!(wire["questions"].is_object(), "questions are keyed by name");

        assert_eq!(wire["questions"]["collides"]["type"], json!("noul"));
        assert_eq!(
            wire["questions"]["collides"]["instructions"],
            json!("will these collide?")
        );
        // A noul declares no criteria, and must not carry an empty one.
        assert!(wire["questions"]["collides"].get("criteria").is_none());

        assert_eq!(wire["questions"]["vendor"]["type"], json!("choice"));
        assert_eq!(
            wire["questions"]["vendor"]["criteria"],
            json!({"openai": null, "anthropic": null}),
            "choice criteria map option names to descriptions"
        );

        assert_eq!(wire["questions"]["urgency"]["type"], json!("score"));
        assert_eq!(
            wire["questions"]["urgency"]["criteria"],
            json!(["1", "2", "3"]),
            "score criteria are an ordered level list, not a min/max pair"
        );
    }

    #[tokio::test]
    async fn the_server_receives_exactly_that_body() {
        // A schema assertion on our own struct could still be undone by the serializer,
        // so the body is matched at the socket.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .and(body_json(json!({
                "state": "task A: rename the parser\ntask B: add a cache",
                "model": "jev-latest",
                "questions": {
                    "q": {"type": "noul", "instructions": "will these collide?"}
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "gliformer-large-v1",
                "answers": {"q": {"type": "noul", "noul": 0.82}},
                "usage": {"input_tokens": 140, "output_tokens": 1}
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        client
            .decide(
                &state(),
                &[Question::noul("q", "will these collide?")],
                &token,
            )
            .await
            .expect("decide");
    }

    #[tokio::test]
    async fn a_noul_batch_round_trips_and_is_accounted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "gliformer-large-v1",
                "answers": {
                    "a": {"type": "noul", "noul": 0.82},
                    "b": {"type": "noul", "noul": 0.11}
                },
                "usage": {"input_tokens": 140, "output_tokens": 2}
            })))
            .mount(&server)
            .await;

        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let questions = vec![
            Question::noul("a", "will these land in the same region?"),
            Question::noul("b", "does this brief fit one worker?"),
        ];
        let answers = client
            .decide(&state(), &questions, &token)
            .await
            .expect("decide");

        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].is_yes(0.5), Some(true));
        assert_eq!(answers[1].is_yes(0.5), Some(false));
        // The format publishes no confidence for a noul, and none is invented.
        assert_eq!(answers[0].confidence(), None);

        // A local decision is still a counted call (every token is attributed to the ledger), with the server's own
        // token figure where it supplies one.
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
        assert_eq!(record.usage.uncached_input, 140);
    }

    #[tokio::test]
    async fn answers_are_matched_by_name_not_by_arrival_order() {
        // A map has no order to rely on, so this is the property that makes the batch
        // safe rather than a nicety.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {
                    "second": {"type": "noul", "noul": 0.9},
                    "first": {"type": "noul", "noul": 0.1}
                }
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let questions = vec![Question::noul("first", "?"), Question::noul("second", "?")];
        let answers = client
            .decide(&state(), &questions, &token)
            .await
            .expect("decide");
        assert_eq!(answers[0].is_yes(0.5), Some(false), "`first` must get 0.1");
        assert_eq!(answers[1].is_yes(0.5), Some(true), "`second` must get 0.9");
    }

    #[tokio::test]
    async fn a_batch_with_a_duplicate_question_id_never_reaches_the_wire() {
        // Names key both directions, so a duplicate id would drop one question and
        // answer a different one under its name. Nothing about the result would look
        // wrong, which is the worst shape a bug can have here.
        let server = MockServer::start().await;
        // No mock mounted: any request fails the test.
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let err = client
            .decide(
                &state(),
                &[Question::noul("same", "?"), Question::noul("same", "?")],
                &token,
            )
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, DecideError::DuplicateQuestionId { .. }),
            "got {err:?}"
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
        // And the row closes rather than dangling.
        assert!(ledger.unreconciled().expect("query").is_empty());
    }

    #[tokio::test]
    async fn an_answer_under_a_name_that_was_never_asked_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"someone-elses-question": {"type": "noul", "noul": 0.7}}
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let err = client
            .decide(&state(), &[Question::noul("mine", "?")], &token)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, DecideError::MissingAnswer { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_of_the_wrong_declared_type_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"n": {"type": "score", "score": 1.0, "confidence": 0.9}}
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let err = client
            .decide(&state(), &[Question::noul("n", "?")], &token)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, DecideError::OutsideAnswerSpace { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_choice_distribution_is_reordered_into_the_callers_option_order() {
        // The wire keys probabilities by option name. Reading them in arrival order
        // would attach each probability to the wrong option.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"c": {
                    "type": "choice",
                    "choice": "anthropic",
                    "confidence": 0.8,
                    "probabilities": {"anthropic": 0.7, "openai": 0.3}
                }}
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let questions = vec![
            Question::choice(
                "c",
                "which vendor",
                vec!["openai".into(), "anthropic".into()],
            )
            .expect("q"),
        ];
        let answers = client
            .decide(&state(), &questions, &token)
            .await
            .expect("decide");
        match &answers[0] {
            Answer::Choice {
                index,
                probabilities,
                ..
            } => {
                assert_eq!(*index, 1, "`anthropic` is the caller's second option");
                assert_eq!(probabilities, &vec![0.3, 0.7], "in the caller's order");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_named_choice_outside_the_option_list_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"c": {"type": "choice", "choice": "a-vendor-we-never-offered",
                                  "confidence": 0.9, "probabilities": {}}}
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let questions =
            vec![Question::choice("c", "which", vec!["a".into(), "b".into()]).expect("q")];
        assert!(client.decide(&state(), &questions, &token).await.is_err());
    }

    #[tokio::test]
    async fn a_score_expectation_is_mapped_back_onto_the_callers_range() {
        // The server answers with an expectation over level indices: 1.7 across three
        // levels is the middle-to-top of the list, not the number 1.7 in the caller's
        // 2..=7 range.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"s": {"type": "score", "score": 1.7, "confidence": 0.6,
                                  "legend": {"0": "2", "1": "3", "2": "4"},
                                  "probabilities": {"0": 0.1, "1": 0.2, "2": 0.7}}}
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let questions = vec![Question::score("s", "rate", 2, 4).expect("q")];
        let answers = client
            .decide(&state(), &questions, &token)
            .await
            .expect("decide");
        assert!(
            matches!(answers[0], Answer::Score { value: 4, .. }),
            "got {:?}",
            answers[0]
        );
    }

    #[test]
    fn a_score_out_of_the_servers_own_level_range_is_clamped_not_wrapped() {
        // Defensive, because a value outside the declared space is the one thing this
        // model class is claimed to be incapable of (fail loud).
        assert_eq!(SystemOneClient::score_from_wire(1, 5, 99.0), 5);
        assert_eq!(SystemOneClient::score_from_wire(1, 5, -4.0), 1);
        assert_eq!(SystemOneClient::score_from_wire(1, 5, f64::NAN), 1);
    }

    #[tokio::test]
    async fn oversized_state_never_reaches_the_server() {
        let server = MockServer::start().await;
        // No mock mounted: any request fails the test.
        let client = SystemOneClient::new(server.uri(), 64).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        let big = State::new("slice", "x".repeat(10_000));
        let err = client
            .decide(&big, &[Question::noul("q", "?")], &token)
            .await
            .expect_err("must refuse");
        assert!(matches!(err, DecideError::StateTooLarge { .. }));
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_dead_server_closes_the_ledger_row_as_failed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        assert!(
            client
                .decide(&state(), &[Question::noul("q", "?")], &token)
                .await
                .is_err()
        );
        assert_eq!(
            ledger
                .call(token.id())
                .expect("read")
                .expect("present")
                .settlement,
            Settlement::Failed
        );
        assert!(ledger.unreconciled().expect("query").is_empty());
    }

    #[tokio::test]
    async fn a_bearer_key_is_sent_when_the_user_has_configured_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::header("authorization", "Bearer devkey"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"q": {"type": "noul", "noul": 0.5}}
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192)
            .expect("client")
            .with_api_key("devkey");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        client
            .decide(&state(), &[Question::noul("q", "?")], &token)
            .await
            .expect("decide");
    }

    #[tokio::test]
    async fn a_missing_usage_block_falls_back_to_the_local_estimate_and_says_so() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"q": {"type": "noul", "noul": 0.5}}
            })))
            .mount(&server)
            .await;
        let client = SystemOneClient::new(server.uri(), 8192).expect("client");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger_token(&ledger);
        client
            .decide(&state(), &[Question::noul("q", "?")], &token)
            .await
            .expect("decide");
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert!(record.usage.uncached_input > 0);
        // The absent raw blob is what marks the figure as locally synthesized rather
        // than passed off as the server's own.
        assert!(record.usage.raw.is_none());
    }
}
