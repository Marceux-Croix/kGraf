//! The question and answer types.
//!
//! Three primitives, matching what the implementations in this space converged on: a
//! yes/no probability, a pick-one over a bounded option list, and an ordinal score.
//! They are deliberately the whole vocabulary. A question that does not fit one of
//! them wants a generative model, and pretending otherwise is how a decision layer
//! turns back into a chat layer.

use std::fmt;

/// The state a question is asked about.
///
/// Unstructured text or JSON. kGraf assembles this from graph slices like any
/// other context (agents read graph slices, not whole files), which matters because a decider's context window is far
/// smaller than a frontier model's, the small local models in this class sit around
/// 8k tokens, so a slice that fits a worker will not always fit a decider.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// A short label, so a recorded decision is legible later.
    pub label: String,
    /// The state itself.
    pub body: String,
}

impl State {
    /// State with a label.
    pub fn new(label: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            body: body.into(),
        }
    }

    /// Rough token cost, reusing the estimator the graph slices use so a decider's
    /// budget is measured the same way everything else is.
    pub fn estimated_tokens(&self) -> u64 {
        kgraf_core::estimate_tokens(&self.body) + kgraf_core::estimate_tokens(&self.label)
    }
}

/// What kind of answer a question admits.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnswerSpace {
    /// Yes or no, answered as a probability of yes.
    Noul,
    /// Pick exactly one of these options.
    ///
    /// Bounded because every implementation surveyed bounds it, in the range of a few
    /// dozen to 255 options. The bound is checked at construction rather than
    /// discovered as a server error.
    Choice { options: Vec<String> },
    /// An ordinal score between `min` and `max` inclusive.
    Score { min: i32, max: i32 },
}

/// The largest option list this crate will send.
///
/// The most permissive implementation surveyed accepts 255; several stop far lower.
/// Refusing at the low end of the observed range keeps a question portable across
/// backends instead of working on one and failing on another.
pub const MAX_CHOICE_OPTIONS: usize = 62;

/// A question about supplied state.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Question {
    /// Stable identifier, so an answer can be matched to its question in a batch.
    pub id: String,
    /// The question, in plain words.
    pub prompt: String,
    pub space: AnswerSpace,
}

/// Why a question could not be built.
#[derive(Debug, thiserror::Error)]
pub enum QuestionError {
    /// The option list is empty, has one element, or exceeds the portable bound.
    #[error("a choice needs 2 to {max} options, got {got}")]
    ChoiceSize { got: usize, max: usize },
    /// Duplicate options make the answer ambiguous.
    #[error("choice options must be distinct; `{duplicate}` appears twice")]
    DuplicateOption { duplicate: String },
    /// An inverted or single-valued score range.
    #[error("a score needs min < max, got {min}..{max}")]
    ScoreRange { min: i32, max: i32 },
    /// A score range with more levels than the wire format renders as labels.
    #[error("a score range may span at most {limit} levels, got {min}..{max}")]
    ScoreSpan { min: i32, max: i32, limit: i64 },
}

/// The most levels a score question may span. Each level is one label on the
/// wire, so an unbounded range is an unbounded request body.
pub const MAX_SCORE_LEVELS: i64 = 1000;

impl Question {
    /// A yes/no question.
    pub fn noul(id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            prompt: prompt.into(),
            space: AnswerSpace::Noul,
        }
    }

    /// A pick-one question, refusing an option list no backend would accept.
    pub fn choice(
        id: impl Into<String>,
        prompt: impl Into<String>,
        options: Vec<String>,
    ) -> Result<Self, QuestionError> {
        if options.len() < 2 || options.len() > MAX_CHOICE_OPTIONS {
            return Err(QuestionError::ChoiceSize {
                got: options.len(),
                max: MAX_CHOICE_OPTIONS,
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for option in &options {
            if !seen.insert(option.as_str()) {
                return Err(QuestionError::DuplicateOption {
                    duplicate: option.clone(),
                });
            }
        }
        Ok(Self {
            id: id.into(),
            prompt: prompt.into(),
            space: AnswerSpace::Choice { options },
        })
    }

    /// An ordinal score question.
    pub fn score(
        id: impl Into<String>,
        prompt: impl Into<String>,
        min: i32,
        max: i32,
    ) -> Result<Self, QuestionError> {
        if min >= max {
            return Err(QuestionError::ScoreRange { min, max });
        }
        if i64::from(max) - i64::from(min) >= MAX_SCORE_LEVELS {
            return Err(QuestionError::ScoreSpan {
                min,
                max,
                limit: MAX_SCORE_LEVELS,
            });
        }
        Ok(Self {
            id: id.into(),
            prompt: prompt.into(),
            space: AnswerSpace::Score { min, max },
        })
    }
}

/// How sure the model is, in `0.0..=1.0`.
///
/// Confidence is a **routing** signal, not a quality score: the intended use is to act
/// on the confident tail, review the middle, and hand the rest to a person. Whether
/// the number deserves that trust is an empirical question, which is what
/// [`crate::calibration`] answers.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Confidence(f64);

impl Confidence {
    /// Clamp into range. A backend reporting something outside `0..=1` is reporting a
    /// bug, and clamping keeps one bad row from poisoning an aggregate.
    pub fn new(value: f64) -> Self {
        Confidence(if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.0
        })
    }

    /// The value.
    pub fn value(self) -> f64 {
        self.0
    }

    /// Whether this clears a routing threshold.
    pub fn at_least(self, threshold: f64) -> bool {
        self.0 >= threshold
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3}", self.0)
    }
}

/// One answer.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    /// Probability of yes, and a confidence **only if the backend published one**.
    ///
    /// The `/v1/systemone` wire format publishes no confidence for a yes/no question:
    /// the probability *is* the answer, and `choice` and `score` are the only shapes
    /// that carry a separate peakedness figure. So this is an `Option`, and a backend
    /// that reports nothing reports nothing. Synthesizing one from the probability,
    /// `|2p - 1|` for instance, would manufacture exactly the decorative confidence
    /// decision 0042 accepts this seam on the absence of.
    Noul {
        p_yes: f64,
        confidence: Option<Confidence>,
    },
    /// The chosen index, the full distribution, and confidence.
    ///
    /// The distribution is kept rather than reduced to the winner, per decision 0020's
    /// general principle: a value discarded at the boundary cannot be recovered by a
    /// consumer that wanted a different operating point.
    Choice {
        index: usize,
        probabilities: Vec<f64>,
        confidence: Confidence,
    },
    /// The score, plus confidence.
    Score { value: i32, confidence: Confidence },
}

impl Answer {
    /// The confidence attached to this answer, where the backend published one.
    ///
    /// `None` for a noul from a backend that publishes no confidence for one, which is
    /// every `/v1/systemone` speaker: absence is reported rather than filled in (fail loud).
    pub fn confidence(&self) -> Option<Confidence> {
        match self {
            Answer::Noul { confidence, .. } => *confidence,
            Answer::Choice { confidence, .. } | Answer::Score { confidence, .. } => {
                Some(*confidence)
            }
        }
    }

    /// For a noul, whether the answer is yes at the given decision threshold.
    ///
    /// The threshold is explicit rather than fixed at 0.5, because the right operating
    /// point depends on the relative cost of the two errors and is the caller's to
    /// choose.
    pub fn is_yes(&self, threshold: f64) -> Option<bool> {
        match self {
            Answer::Noul { p_yes, .. } => Some(*p_yes >= threshold),
            _ => None,
        }
    }

    /// Whether this answer's shape matches the space that was asked for.
    ///
    /// A backend returning a Score for a Noul question is malformed, and "cannot
    /// return outside the declared space" is a claim to verify rather than assume
    /// (fail loud).
    pub fn fits(&self, space: &AnswerSpace) -> bool {
        match (self, space) {
            (Answer::Noul { p_yes, .. }, AnswerSpace::Noul) => (0.0..=1.0).contains(p_yes),
            (
                Answer::Choice {
                    index,
                    probabilities,
                    ..
                },
                AnswerSpace::Choice { options },
            ) => {
                *index < options.len()
                    && (probabilities.is_empty() || probabilities.len() == options.len())
            }
            (Answer::Score { value, .. }, AnswerSpace::Score { min, max }) => {
                (*min..=*max).contains(value)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_refuses_an_option_list_no_backend_would_accept() {
        assert!(Question::choice("q", "pick", vec!["only".into()]).is_err());
        let too_many: Vec<String> = (0..MAX_CHOICE_OPTIONS + 1).map(|i| i.to_string()).collect();
        assert!(Question::choice("q", "pick", too_many).is_err());
        assert!(Question::choice("q", "pick", vec!["a".into(), "b".into()]).is_ok());
    }

    #[test]
    fn duplicate_options_are_refused_because_the_answer_would_be_ambiguous() {
        let err =
            Question::choice("q", "pick", vec!["a".into(), "a".into()]).expect_err("must refuse");
        assert!(matches!(err, QuestionError::DuplicateOption { .. }));
    }

    #[test]
    fn an_inverted_score_range_is_refused() {
        assert!(Question::score("q", "rate", 5, 1).is_err());
        assert!(Question::score("q", "rate", 1, 1).is_err());
        assert!(Question::score("q", "rate", 1, 5).is_ok());
    }

    #[test]
    fn an_answer_of_the_wrong_shape_does_not_fit_its_question() {
        // "Cannot return outside the declared answer space" is a claim to check, not
        // to assume.
        let noul = AnswerSpace::Noul;
        assert!(
            Answer::Noul {
                p_yes: 0.7,
                confidence: None
            }
            .fits(&noul)
        );
        assert!(
            !Answer::Score {
                value: 3,
                confidence: Confidence::new(0.9)
            }
            .fits(&noul)
        );
    }

    #[test]
    fn an_out_of_range_choice_index_does_not_fit() {
        let space = AnswerSpace::Choice {
            options: vec!["a".into(), "b".into()],
        };
        assert!(
            !Answer::Choice {
                index: 7,
                probabilities: vec![],
                confidence: Confidence::new(0.5)
            }
            .fits(&space)
        );
    }

    #[test]
    fn confidence_is_clamped_rather_than_trusted() {
        assert_eq!(Confidence::new(1.7).value(), 1.0);
        assert_eq!(Confidence::new(-0.2).value(), 0.0);
        assert_eq!(Confidence::new(f64::NAN).value(), 0.0);
    }

    #[test]
    fn the_noul_decision_threshold_is_the_callers_choice() {
        let answer = Answer::Noul {
            p_yes: 0.6,
            confidence: None,
        };
        assert_eq!(answer.is_yes(0.5), Some(true));
        assert_eq!(answer.is_yes(0.7), Some(false));
    }
}
