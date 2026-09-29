//! The decider seam.
//!
//! Deliberately shaped like the provider seam and deliberately not the same trait. A
//! provider returns text and is billed per token; a decider returns a typed value in a
//! declared space and, locally, costs no money at all. Folding them together would
//! mean one of the two lies about what it returns.
//!
//! What they share is the accounting. [`Decider::decide`] takes the same ledger-minted
//! `CallToken` that `Provider::complete` does, so the rule that every token is attributed covers a local model exactly
//! as it covers a hosted one, and decision 0039's guarantee extends here for free: a
//! decision that is not counted does not compile.

use crate::decision::{Answer, Question, State};
use kgraf_provider::CallToken;

/// Which execution provider a backend is actually using.
///
/// **CPU is the floor and everything else is opportunistic.** kGraf ships to
/// arbitrary consumer machines on macOS, Windows and Linux (decision 0002), so a
/// decider that requires one vendor's GPU is not a decider kGraf can have, the
/// heavyweight implementations in this space need an NVIDIA card and a container
/// runtime, which rules out every Apple machine and most laptops.
///
/// Reported rather than assumed, because a silently absent accelerator is a tenfold
/// slowdown that presents as "this feels sluggish". The fail-loud rule makes the absence of a
/// capability something to state; here the fallback is *correct behavior* and the
/// statement is what keeps it honest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Acceleration {
    /// Plain CPU. Always available, and the only thing guaranteed to be.
    Cpu,
    /// Apple's CoreML or Metal path.
    AppleGpu,
    /// Windows DirectML, which covers AMD and Intel GPUs as well as NVIDIA.
    DirectMl,
    /// NVIDIA CUDA.
    Cuda,
    /// AMD ROCm.
    Rocm,
    /// A remote server is doing the work, so local acceleration is not applicable.
    Remote,
}

impl Acceleration {
    /// The name, for display and for the recorded decision.
    pub fn as_str(self) -> &'static str {
        match self {
            Acceleration::Cpu => "cpu",
            Acceleration::AppleGpu => "apple-gpu",
            Acceleration::DirectMl => "directml",
            Acceleration::Cuda => "cuda",
            Acceleration::Rocm => "rocm",
            Acceleration::Remote => "remote",
        }
    }

    /// Whether this is the guaranteed-available floor.
    pub fn is_floor(self) -> bool {
        self == Acceleration::Cpu
    }
}

/// Decider failures.
#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    /// The backend could not be reached.
    #[error("decider `{backend}` unreachable: {reason}")]
    Unreachable {
        backend: &'static str,
        reason: String,
    },

    /// The backend's reply could not be decoded.
    #[error("cannot decode `{backend}` reply: {reason}")]
    Decode {
        backend: &'static str,
        reason: String,
    },

    /// The backend answered outside the declared answer space.
    ///
    /// The whole premise of this class of model is that it cannot do this. That premise
    /// is checked rather than trusted, because "cannot return outside your schema" is a
    /// claim about the vendor's implementation and not a law (fail loud).
    #[error("`{backend}` answered outside the declared answer space for question `{question}`")]
    OutsideAnswerSpace {
        backend: &'static str,
        question: String,
    },

    /// A question came back unanswered, or an answer arrived for a question not asked.
    #[error("`{backend}` returned {returned} answers for {asked} questions")]
    AnswerCountMismatch {
        backend: &'static str,
        asked: usize,
        returned: usize,
    },

    /// Two questions in one batch share an id.
    ///
    /// The wire format keys both the question block and the answer block by name, so a
    /// duplicate id silently drops a question and answers a different one in its place.
    /// Refused before anything is sent.
    #[error("question id `{id}` appears twice in one batch; ids key the wire format")]
    DuplicateQuestionId { backend: &'static str, id: String },

    /// The reply carries no answer under a question's id.
    #[error("`{backend}` returned no answer for question `{question}`")]
    MissingAnswer {
        backend: &'static str,
        question: String,
    },

    /// The state exceeds what this backend can read.
    ///
    /// Small local models in this class sit near 8k tokens, well under a frontier
    /// model's window, so a slice sized for a worker will not always fit a decider.
    /// Truncating silently would answer a different question than the one asked.
    #[error("state is about {estimated} tokens, over `{backend}`'s {limit} limit")]
    StateTooLarge {
        backend: &'static str,
        estimated: u64,
        limit: u64,
    },

    /// The ledger refused.
    #[error("accounting failure")]
    Accounting(#[from] kgraf_provider::LedgerError),
}

/// Something that answers typed questions about state.
#[async_trait::async_trait]
pub trait Decider: Send + Sync {
    /// The backend's stable name.
    fn backend(&self) -> &'static str;

    /// Which execution provider is in use.
    fn acceleration(&self) -> Acceleration;

    /// The largest state this backend will read, in estimated tokens.
    fn state_token_limit(&self) -> u64;

    /// Answer every question about this state, in one pass.
    ///
    /// Batched because the whole speed argument for this class of model rests on
    /// answering a block of questions in one forward pass rather than one at a time.
    /// Implementations must return exactly one answer per question, in order, and must
    /// verify each answer fits the space it was asked in.
    async fn decide(
        &self,
        state: &State,
        questions: &[Question],
        token: &CallToken,
    ) -> Result<Vec<Answer>, DecideError>;

    /// Answer one yes/no question, as a convenience over [`Decider::decide`].
    async fn ask_noul(
        &self,
        state: &State,
        prompt: &str,
        token: &CallToken,
    ) -> Result<Answer, DecideError> {
        let question = Question::noul("q0", prompt);
        let mut answers = self
            .decide(state, std::slice::from_ref(&question), token)
            .await?;
        if answers.len() != 1 {
            return Err(DecideError::AnswerCountMismatch {
                backend: self.backend(),
                asked: 1,
                returned: answers.len(),
            });
        }
        Ok(answers.remove(0))
    }
}

/// Check a batch of answers against the questions asked.
///
/// Shared by every backend so the verification cannot be forgotten in one of them.
pub fn verify_answers(
    backend: &'static str,
    questions: &[Question],
    answers: &[Answer],
) -> Result<(), DecideError> {
    if questions.len() != answers.len() {
        return Err(DecideError::AnswerCountMismatch {
            backend,
            asked: questions.len(),
            returned: answers.len(),
        });
    }
    for (question, answer) in questions.iter().zip(answers) {
        if !answer.fits(&question.space) {
            return Err(DecideError::OutsideAnswerSpace {
                backend,
                question: question.id.clone(),
            });
        }
    }
    Ok(())
}

/// Check a state against a backend's window before sending it.
pub fn verify_state_size(
    backend: &'static str,
    state: &State,
    limit: u64,
) -> Result<(), DecideError> {
    let estimated = state.estimated_tokens();
    if estimated > limit {
        return Err(DecideError::StateTooLarge {
            backend,
            estimated,
            limit,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{AnswerSpace, Confidence};

    #[test]
    fn cpu_is_the_only_acceleration_that_is_guaranteed() {
        assert!(Acceleration::Cpu.is_floor());
        for accelerated in [
            Acceleration::AppleGpu,
            Acceleration::DirectMl,
            Acceleration::Cuda,
            Acceleration::Rocm,
            Acceleration::Remote,
        ] {
            assert!(
                !accelerated.is_floor(),
                "{} must not be treated as guaranteed",
                accelerated.as_str()
            );
        }
    }

    #[test]
    fn a_missing_answer_is_caught_rather_than_silently_shortening_the_batch() {
        let questions = vec![Question::noul("a", "?"), Question::noul("b", "?")];
        let answers = vec![Answer::Noul {
            p_yes: 0.5,
            confidence: None,
        }];
        let err = verify_answers("test", &questions, &answers).expect_err("must raise");
        assert!(matches!(err, DecideError::AnswerCountMismatch { .. }));
    }

    #[test]
    fn an_answer_outside_the_declared_space_is_caught() {
        // The premise of this model class is that this cannot happen. Verified, not
        // trusted.
        let questions = vec![Question::score("s", "rate", 1, 5).expect("q")];
        let answers = vec![Answer::Score {
            value: 99,
            confidence: Confidence::new(0.9),
        }];
        let err = verify_answers("test", &questions, &answers).expect_err("must raise");
        assert!(matches!(err, DecideError::OutsideAnswerSpace { .. }));
    }

    #[test]
    fn a_mismatched_answer_type_is_caught() {
        let questions = vec![Question::noul("n", "?")];
        let answers = vec![Answer::Score {
            value: 1,
            confidence: Confidence::new(0.9),
        }];
        assert!(verify_answers("test", &questions, &answers).is_err());
    }

    #[test]
    fn oversized_state_raises_rather_than_being_truncated() {
        // Truncating would answer a different question than the one asked, and the
        // caller would never know.
        let big = State::new("slice", "x".repeat(100_000));
        let err = verify_state_size("test", &big, 8_192).expect_err("must raise");
        assert!(matches!(err, DecideError::StateTooLarge { .. }));
        let small = State::new("slice", "short");
        assert!(verify_state_size("test", &small, 8_192).is_ok());
    }

    #[test]
    fn a_well_formed_batch_verifies() {
        let questions = vec![
            Question::noul("n", "?"),
            Question::choice("c", "pick", vec!["a".into(), "b".into()]).expect("q"),
        ];
        let answers = vec![
            Answer::Noul {
                p_yes: 0.2,
                confidence: None,
            },
            Answer::Choice {
                index: 1,
                probabilities: vec![0.3, 0.7],
                confidence: Confidence::new(0.7),
            },
        ];
        verify_answers("test", &questions, &answers).expect("valid");
        assert!(matches!(questions[1].space, AnswerSpace::Choice { .. }));
    }
}
