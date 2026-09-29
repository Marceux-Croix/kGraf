//! Whether a decider is installed, and what happens when it is not.
//!
//! The decider is **optional**. kGraf runs without it, because a user may have no
//! network, no disk to spare, or no wish to run a model they did not ask for. Installing
//! it is strongly encouraged and is not required.
//!
//! That is a genuinely dangerous shape, and the danger is the exact failure the fail-loud rule was
//! written about: a reference implementation had two headline features dead for months
//! with zero signal, because an absent binary read as a no-op and a missing directory
//! read as "nothing yet". An optional decider that quietly returns a default when absent
//! would reproduce that precisely, every decision would still be *made*, just worse,
//! and nothing would say so.
//!
//! Three rules prevent it.
//!
//! **There is no null decider.** This module deliberately contains no implementation of
//! [`crate::Decider`] that answers without a model. Absence is represented by a slot
//! that cannot be asked, not by a backend that always says no.
//!
//! **Absence is declared, not inferred.** A [`DeciderSlot`] says *why* it is empty,
//! never installed, declined by the user, or failed, and carries what to do about it.
//! This is the same distinction `ProviderConfig::local` draws for a credential that is
//! legitimately absent versus one that was forgotten (decision 0041).
//!
//! **Every decision records who made it.** [`Decided`] is either a model's answer with
//! its confidence, or a fallback with the reason the fallback was used. A caller cannot
//! obtain a value without also obtaining that provenance, so "was this decided or
//! defaulted?" is answerable from the record rather than from memory, which is
//! decision 0036's test applied here: the answer checks against a recorded artifact.

use crate::decider::Decider;

/// Why no decider is available.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Absence {
    /// Never installed. The normal state on a fresh machine, and the one the interface
    /// should offer to fix.
    NotInstalled,
    /// The user was offered it and said no. Recorded distinctly from `NotInstalled` so
    /// the offer is not made again, which is the difference between encouraging and
    /// nagging.
    Declined,
    /// Installation or loading failed, with the reason kept verbatim.
    ///
    /// Distinct from `NotInstalled` because it is a fault rather than a state, and a
    /// fault should be visible until it is resolved.
    Failed { reason: String },
}

impl Absence {
    /// A short sentence for the interface, ending in what the user can do.
    pub fn explain(&self) -> String {
        match self {
            Absence::NotInstalled => "No local decision model is installed. kGraf works \
                 without one and makes these choices by fixed rule instead; installing it \
                 lets them be judged."
                .to_owned(),
            Absence::Declined => "The local decision model was declined. Fixed rules are \
                 being used. This can be changed in settings."
                .to_owned(),
            Absence::Failed { reason } => format!(
                "The local decision model could not be loaded: {reason}. Fixed rules are \
                 being used until this is resolved."
            ),
        }
    }

    /// Whether this absence should prompt an offer to install.
    ///
    /// A declined model is not re-offered, and a failure is reported rather than
    /// re-offered, because offering again is how encouragement becomes nagging.
    pub fn should_offer_install(&self) -> bool {
        matches!(self, Absence::NotInstalled)
    }
}

/// A decider, or a stated reason there is none.
///
/// Deliberately not `Option<Box<dyn Decider>>`. An `Option` invites `unwrap_or_default`,
/// and there is no default here that would be honest.
pub enum DeciderSlot {
    /// A decider is loaded and can be asked.
    Ready(Box<dyn Decider>),
    /// No decider, and why.
    Absent(Absence),
}

impl DeciderSlot {
    /// An empty slot on a fresh machine.
    pub fn not_installed() -> Self {
        DeciderSlot::Absent(Absence::NotInstalled)
    }

    /// An empty slot the user chose.
    pub fn declined() -> Self {
        DeciderSlot::Absent(Absence::Declined)
    }

    /// An empty slot because something broke.
    pub fn failed(reason: impl Into<String>) -> Self {
        DeciderSlot::Absent(Absence::Failed {
            reason: reason.into(),
        })
    }

    /// The decider, if there is one.
    pub fn decider(&self) -> Option<&dyn Decider> {
        match self {
            DeciderSlot::Ready(decider) => Some(decider.as_ref()),
            DeciderSlot::Absent(_) => None,
        }
    }

    /// Why there is none, if there is none.
    pub fn absence(&self) -> Option<&Absence> {
        match self {
            DeciderSlot::Ready(_) => None,
            DeciderSlot::Absent(absence) => Some(absence),
        }
    }

    /// Whether a decision can be asked for at all.
    pub fn is_ready(&self) -> bool {
        matches!(self, DeciderSlot::Ready(_))
    }
}

/// How a value was arrived at.
///
/// Every consumer of a decision receives this rather than a bare value, so provenance
/// cannot be dropped on the floor. A graph that draws a scheduling choice can say whether
/// a model judged it or a rule produced it, and a user asking "why did it do that" gets an
/// answer that does not depend on anyone having remembered to log it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Decided<T> {
    /// A model answered, with the confidence it reported.
    ByModel {
        value: T,
        confidence: crate::decision::Confidence,
        backend: String,
    },
    /// No model answered. The value came from the named rule, for the stated reason.
    ByFallback {
        value: T,
        /// The rule that produced the value, named at the call site.
        rule: String,
        /// Why no model was consulted.
        why: Absence,
    },
}

impl<T> Decided<T> {
    /// The value, whichever way it was reached.
    pub fn value(&self) -> &T {
        match self {
            Decided::ByModel { value, .. } | Decided::ByFallback { value, .. } => value,
        }
    }

    /// Consume and return the value.
    pub fn into_value(self) -> T {
        match self {
            Decided::ByModel { value, .. } | Decided::ByFallback { value, .. } => value,
        }
    }

    /// Whether a model actually judged this.
    pub fn is_judged(&self) -> bool {
        matches!(self, Decided::ByModel { .. })
    }

    /// Whether a model judged it **and** cleared a confidence threshold.
    ///
    /// The distinction a gate would need: an unjudged value and a judged-but-uncertain
    /// value are both "do not rely on this", and collapsing them would let a fallback
    /// masquerade as a low-confidence judgement.
    pub fn is_confident(&self, threshold: f64) -> bool {
        match self {
            Decided::ByModel { confidence, .. } => confidence.at_least(threshold),
            Decided::ByFallback { .. } => false,
        }
    }

    /// A one-line provenance note, for a transcript or a node label.
    pub fn provenance(&self) -> String {
        match self {
            Decided::ByModel {
                confidence,
                backend,
                ..
            } => format!("judged by {backend} at confidence {confidence}"),
            Decided::ByFallback { rule, why, .. } => match why {
                Absence::NotInstalled => format!("rule `{rule}` (no model installed)"),
                Absence::Declined => format!("rule `{rule}` (model declined)"),
                Absence::Failed { reason } => format!("rule `{rule}` (model failed: {reason})"),
            },
        }
    }
}

/// Build a fallback decision, naming the rule that produced it.
///
/// The rule name is required rather than optional, because an unnamed fallback is
/// indistinguishable from a silent default six months later.
pub fn fell_back<T>(value: T, rule: impl Into<String>, why: &Absence) -> Decided<T> {
    Decided::ByFallback {
        value,
        rule: rule.into(),
        why: why.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::Confidence;
    use crate::fake::FakeDecider;

    #[test]
    fn there_is_no_decider_implementation_that_answers_without_a_model() {
        // The rule this module exists to hold. An absent decider must be unaskable, not a
        // backend that always says no: that is the "absent binary reads as a no-op"
        // failure the fail-loud rule targets.
        let slot = DeciderSlot::not_installed();
        assert!(slot.decider().is_none());
        assert!(!slot.is_ready());
    }

    #[test]
    fn an_absent_decider_states_which_kind_of_absence_it_is() {
        // Never-installed, declined and broken are three different facts and only one of
        // them should produce an offer.
        assert!(Absence::NotInstalled.should_offer_install());
        assert!(!Absence::Declined.should_offer_install());
        assert!(
            !Absence::Failed {
                reason: "checksum mismatch".into()
            }
            .should_offer_install()
        );
    }

    #[test]
    fn every_absence_explains_itself_and_says_what_is_happening_instead() {
        for absence in [
            Absence::NotInstalled,
            Absence::Declined,
            Absence::Failed {
                reason: "no disk space".into(),
            },
        ] {
            let text = absence.explain();
            assert!(
                text.contains("rule") || text.contains("Fixed rules"),
                "an absence must say what is used instead: {text}"
            );
        }
    }

    #[test]
    fn a_fallback_cannot_be_built_without_naming_its_rule() {
        // Enforced by the signature rather than by convention: `fell_back` takes the rule
        // name, so an unnamed fallback does not compile.
        let decided = fell_back(true, "prefer-cross-feature-pairs", &Absence::NotInstalled);
        assert_eq!(decided.value(), &true);
        assert!(!decided.is_judged());
        assert!(decided.provenance().contains("prefer-cross-feature-pairs"));
    }

    #[test]
    fn a_fallback_is_never_confident_at_any_threshold() {
        // The trap this guards: treating a default as a low-confidence judgement would
        // let it through any gate loose enough to accept uncertainty.
        let decided = fell_back(true, "some-rule", &Absence::Declined);
        for threshold in [0.0, 0.5, 0.9, 1.0] {
            assert!(
                !decided.is_confident(threshold),
                "a fallback cleared threshold {threshold}"
            );
        }
    }

    #[test]
    fn a_judged_decision_carries_its_backend_and_confidence() {
        let decided = Decided::ByModel {
            value: 7u32,
            confidence: Confidence::new(0.91),
            backend: "systemone-http".to_owned(),
        };
        assert!(decided.is_judged());
        assert!(decided.is_confident(0.9));
        assert!(!decided.is_confident(0.95));
        let note = decided.provenance();
        assert!(note.contains("systemone-http"));
        assert!(note.contains("0.91"));
    }

    #[test]
    fn a_ready_slot_exposes_its_decider_and_no_absence() {
        let slot = DeciderSlot::Ready(Box::new(FakeDecider::hashed()));
        assert!(slot.is_ready());
        assert_eq!(slot.decider().map(|d| d.backend()), Some("fake-decider"));
        assert!(slot.absence().is_none());
    }

    #[test]
    fn a_failure_keeps_its_reason_verbatim_so_it_can_be_acted_on() {
        let slot = DeciderSlot::failed("sha256 mismatch: refused to load");
        match slot.absence() {
            Some(Absence::Failed { reason }) => assert!(reason.contains("sha256")),
            other => panic!("wrong absence: {other:?}"),
        }
    }

    #[test]
    fn provenance_distinguishes_the_three_absences_in_its_text() {
        // A user reading "why did it choose that" must be able to tell a missing model
        // from a broken one.
        let not_installed = fell_back(1, "r", &Absence::NotInstalled).provenance();
        let declined = fell_back(1, "r", &Absence::Declined).provenance();
        let failed = fell_back(
            1,
            "r",
            &Absence::Failed {
                reason: "corrupt".into(),
            },
        )
        .provenance();
        assert_ne!(not_installed, declined);
        assert_ne!(declined, failed);
        assert!(failed.contains("corrupt"));
    }
}
