//! A deterministic decider, for tests and for a baseline.
//!
//! Two jobs, and the second is the more important one.
//!
//! It lets this crate's tests and the probe harness run with **no model
//! weights and no network**, which is what keeps the gates green on a machine that has
//! never downloaded a checkpoint.
//!
//! And it is the control. An earlier experiment killed the code graph as a collision predictor by
//! comparing it against a size-matched random baseline, and a later shuffled-label
//! control scored 0.501 against the model's 0.613. A probe without a control measures
//! nothing, so the control ships beside the mechanism rather than being written later
//! by someone who already knows the answer.

use crate::decider::{Acceleration, DecideError, Decider, verify_answers, verify_state_size};
use crate::decision::{Answer, AnswerSpace, Confidence, Question, State};
use kgraf_provider::CallToken;

/// This backend's stable name.
pub const BACKEND: &str = "fake-decider";

/// How a fake decider answers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Always yes, the lowest option, the lowest score. The always-CONFLICT baseline
    /// the collision experiment reports against.
    AlwaysFirst,
    /// Hashed from the state and question id: arbitrary, but identical on every run and
    /// every machine. This is the random control.
    Hashed,
    /// A fixed answer, for testing a specific path.
    Fixed { p_yes: f64, confidence: f64 },
}

/// A decider that calls no model.
pub struct FakeDecider {
    mode: Mode,
    state_token_limit: u64,
}

impl FakeDecider {
    /// A deterministic pseudo-random control.
    pub fn hashed() -> Self {
        Self {
            mode: Mode::Hashed,
            state_token_limit: 8_192,
        }
    }

    /// The always-positive baseline.
    pub fn always_first() -> Self {
        Self {
            mode: Mode::AlwaysFirst,
            state_token_limit: 8_192,
        }
    }

    /// A fixed answer.
    pub fn fixed(p_yes: f64, confidence: f64) -> Self {
        Self {
            mode: Mode::Fixed { p_yes, confidence },
            state_token_limit: 8_192,
        }
    }

    /// Override the state limit, to exercise the oversize path.
    pub fn with_state_token_limit(mut self, limit: u64) -> Self {
        self.state_token_limit = limit;
        self
    }

    /// A stable unit value in `0.0..1.0` from the state and question.
    fn unit(&self, state: &State, question: &Question) -> f64 {
        let digest =
            kgraf_core::ids::stable_id("fake-decider", &[&state.label, &state.body, &question.id]);
        // First 8 hex characters as a fraction: stable across processes and platforms,
        // which a `HashMap`-style hasher would not be.
        let bits = u32::from_str_radix(&digest[..8], 16).unwrap_or(0);
        bits as f64 / u32::MAX as f64
    }
}

#[async_trait::async_trait]
impl Decider for FakeDecider {
    fn backend(&self) -> &'static str {
        BACKEND
    }

    fn acceleration(&self) -> Acceleration {
        // No model runs, so the floor is the honest answer.
        Acceleration::Cpu
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
        let mut answers = Vec::with_capacity(questions.len());
        for question in questions {
            let unit = self.unit(state, question);
            let answer = match (&question.space, self.mode) {
                // The controls publish a noul confidence where a real `/v1/systemone`
                // speaker publishes none. That is deliberate: these are the fixtures the
                // routing path is exercised with, and a control that could not express a
                // confidence would leave that path untested. Nothing compares the two
                // figures: the probe scores `p_yes` alone.
                (AnswerSpace::Noul, Mode::AlwaysFirst) => Answer::Noul {
                    p_yes: 1.0,
                    confidence: Some(Confidence::new(1.0)),
                },
                (AnswerSpace::Noul, Mode::Hashed) => Answer::Noul {
                    p_yes: unit,
                    confidence: Some(Confidence::new((unit - 0.5).abs() * 2.0)),
                },
                (AnswerSpace::Noul, Mode::Fixed { p_yes, confidence }) => Answer::Noul {
                    p_yes,
                    confidence: Some(Confidence::new(confidence)),
                },
                (AnswerSpace::Choice { options }, mode) => {
                    let index = match mode {
                        Mode::AlwaysFirst => 0,
                        _ => ((unit * options.len() as f64) as usize).min(options.len() - 1),
                    };
                    let mut probabilities = vec![0.0; options.len()];
                    probabilities[index] = 1.0;
                    Answer::Choice {
                        index,
                        probabilities,
                        confidence: Confidence::new(unit),
                    }
                }
                (AnswerSpace::Score { min, max }, mode) => {
                    let value = match mode {
                        Mode::AlwaysFirst => *min,
                        _ => {
                            let span = i64::from(*max) - i64::from(*min);
                            (i64::from(*min) + (unit * span as f64).round() as i64)
                                .clamp(i64::from(*min), i64::from(*max))
                                as i32
                        }
                    };
                    Answer::Score {
                        value: value.clamp(*min, *max),
                        confidence: Confidence::new(unit),
                    }
                }
            };
            answers.push(answer);
        }
        verify_answers(BACKEND, questions, &answers)?;
        // Counted like any other decision. A control that bypassed the ledger would make
        // the ledger's totals depend on which arm was running.
        token
            .settle_async(
                &kgraf_core::TokenUsage {
                    uncached_input: state.estimated_tokens(),
                    ..kgraf_core::TokenUsage::default()
                },
                None,
            )
            .await?;
        Ok(answers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::Protocol;
    use kgraf_provider::Ledger;

    fn ledger() -> Ledger {
        Ledger::open_in_memory().expect("ledger")
    }

    #[tokio::test]
    async fn the_hashed_control_is_identical_across_runs() {
        // A control that varies between runs cannot be compared against, and a seeded
        // RNG would still vary with iteration order. Hashing the inputs does not.
        let decider = FakeDecider::hashed();
        let state = State::new("pair", "a and b");
        let questions = vec![Question::noul("q", "?")];
        let mut seen = Vec::new();
        for _ in 0..3 {
            let ledger = ledger();
            let token = ledger
                .begin("fake", Protocol::Inclusive, "m", "a", "t")
                .expect("begin");
            let answers = decider
                .decide(&state, &questions, &token)
                .await
                .expect("decide");
            seen.push(match answers[0] {
                Answer::Noul { p_yes, .. } => p_yes,
                _ => unreachable!(),
            });
        }
        assert_eq!(seen[0], seen[1]);
        assert_eq!(seen[1], seen[2]);
    }

    #[tokio::test]
    async fn the_hashed_control_spreads_across_the_range() {
        // A control pinned near 0.5 would look like a calibrated model rather than a
        // coin flip, and would flatter whatever it was compared against.
        let decider = FakeDecider::hashed();
        let mut values = Vec::new();
        for i in 0..200 {
            let ledger = ledger();
            let token = ledger
                .begin("fake", Protocol::Inclusive, "m", "a", "t")
                .expect("begin");
            let state = State::new("pair", format!("case {i}"));
            let answers = decider
                .decide(&state, &[Question::noul("q", "?")], &token)
                .await
                .expect("decide");
            values.push(match answers[0] {
                Answer::Noul { p_yes, .. } => p_yes,
                _ => unreachable!(),
            });
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        assert!((mean - 0.5).abs() < 0.08, "mean was {mean}");
        assert!(values.iter().any(|v| *v < 0.1), "no low values");
        assert!(values.iter().any(|v| *v > 0.9), "no high values");
    }

    #[tokio::test]
    async fn every_mode_answers_inside_the_declared_space() {
        let questions = vec![
            Question::noul("n", "?"),
            Question::choice("c", "pick", vec!["a".into(), "b".into(), "c".into()]).expect("q"),
            Question::score("s", "rate", 2, 7).expect("q"),
        ];
        for decider in [
            FakeDecider::hashed(),
            FakeDecider::always_first(),
            FakeDecider::fixed(0.75, 0.9),
        ] {
            let ledger = ledger();
            let token = ledger
                .begin("fake", Protocol::Inclusive, "m", "a", "t")
                .expect("begin");
            let state = State::new("pair", "some state");
            let answers = decider
                .decide(&state, &questions, &token)
                .await
                .expect("decide");
            for (question, answer) in questions.iter().zip(&answers) {
                assert!(
                    answer.fits(&question.space),
                    "{} answered outside its space: {answer:?}",
                    decider.backend()
                );
            }
        }
    }

    #[tokio::test]
    async fn the_always_first_baseline_always_says_yes() {
        let ledger = ledger();
        let token = ledger
            .begin("fake", Protocol::Inclusive, "m", "a", "t")
            .expect("begin");
        let answers = FakeDecider::always_first()
            .decide(
                &State::new("pair", "anything"),
                &[Question::noul("q", "?")],
                &token,
            )
            .await
            .expect("decide");
        assert_eq!(answers[0].is_yes(0.5), Some(true));
    }

    #[tokio::test]
    async fn a_control_run_is_still_counted() {
        let ledger = ledger();
        let token = ledger
            .begin("fake", Protocol::Inclusive, "m", "a", "t")
            .expect("begin");
        FakeDecider::hashed()
            .decide(
                &State::new("pair", "anything"),
                &[Question::noul("q", "?")],
                &token,
            )
            .await
            .expect("decide");
        assert_eq!(ledger.calls().expect("calls").len(), 1);
        assert!(ledger.unreconciled().expect("query").is_empty());
    }

    #[tokio::test]
    async fn oversized_state_is_refused_here_too() {
        let ledger = ledger();
        let token = ledger
            .begin("fake", Protocol::Inclusive, "m", "a", "t")
            .expect("begin");
        let decider = FakeDecider::hashed().with_state_token_limit(16);
        let err = decider
            .decide(
                &State::new("slice", "x".repeat(1_000)),
                &[Question::noul("q", "?")],
                &token,
            )
            .await
            .expect_err("must refuse");
        assert!(matches!(err, DecideError::StateTooLarge { .. }));
    }
}
