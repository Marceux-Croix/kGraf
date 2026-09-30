//! Typed decisions from a System-One class model.
//!
//! A generative model answers in prose and is billed per token. A **System-One**
//! model answers a *typed question* about supplied state in one forward pass, and
//! returns a value inside a declared answer space plus a calibrated probability.
//! kGraf wants a great many of those: does this brief fit one worker, will these
//! two changes land in the same region, which vendor should take this part, which of
//! these passing attempts is best.
//!
//! Four rules are enforced here rather than left to the caller.
//!
//! **CPU is the floor; acceleration is opportunistic.** kGraf ships to arbitrary
//! consumer machines on three platforms (decision 0002), so a decider that needs a
//! particular vendor's GPU is not a decider kGraf can have. Every backend must
//! answer on CPU alone, and any accelerator is a speedup that may be absent. Which
//! one is actually in use is **reported**, never assumed, see [`Acceleration`].
//!
//! **A local model is still an accounted call.** The accounting rule says no provider call may be
//! issued outside the accounting path, and a decision is a call. [`Decider::decide`]
//! therefore takes the same ledger-minted `CallToken` a provider does (decisions 0039,
//! 0040), so a local model cannot become an unmeasured back channel. Its price row
//! says zero, which is a statement about money and not about compute.
//!
//! **Confidence is measured, not believed.** Every project in this space claims
//! calibrated probabilities, and calibration is the one property kGraf actually
//! needs: the collision experiment's problem was not that the signal is weak, it is that a weak
//! signal with untrustworthy confidence cannot be gated on. So [`calibration`] scores
//! it, and a probe reports expected calibration error beside accuracy.
//!
//! **Typed does not mean correct.** A value inside the declared answer space can be
//! confidently wrong. The vendor claim that such a model "cannot hallucinate" is a
//! statement about well-formedness, and this project exists partly because a
//! reputation turned out to be an unverified claim read back as a product feature
//! (a typed answer is not a verified one). Nothing here treats a typed answer as a verified one.
//!
//! The wire format is `/v1/systemone`, which several independent implementations
//! already serve, so a user may point kGraf at a heavyweight GPU server for
//! accuracy or run the small local model for reach, without kGraf caring which.
//!
//! # A route like any other (decision 0089)
//!
//! A typed-decision model is declared, credentialed and accounted as any route is:
//! `typed-decision=systemone:<model>` names the catalog's `systemone` route, which
//! `kgraf_fleet::build_kind` builds into a `kgraf_provider::ModelRoute` holding an
//! `Arc<dyn Decider>` (a [`SystemOneClient`]). The wire is unchanged. The orchestrator
//! and the workers reach it through the `decide` tool (`kgraf_tools::model_tools`);
//! nothing calls it by default, and decision 0045 still holds: no decider supplies the
//! collision expectation handshake.

pub mod availability;
pub mod calibration;
pub mod decider;
pub mod decision;
pub mod fake;
pub mod install;
pub mod systemone;

pub use availability::{Absence, Decided, DeciderSlot, fell_back};
pub use calibration::{Calibration, ReliabilityBin};
pub use decider::{Acceleration, DecideError, Decider};
pub use decision::{
    Answer, AnswerSpace, Confidence, MAX_SCORE_LEVELS, Question, QuestionError, State,
};
pub use fake::FakeDecider;
pub use install::{InstallChoice, InstallState, ModelManifest};
pub use systemone::SystemOneClient;

/// The model kind a [`Decider`] serves. A route of this kind holds an
/// `Arc<dyn Decider>` as its handle.
pub const KIND: kgraf_core::ModelKind = kgraf_core::ModelKind::TYPED_DECISION;

#[cfg(test)]
mod kind_tests {
    use super::*;
    use kgraf_core::Protocol;
    use kgraf_provider::{Ledger, ModelRoute, Settlement};
    use serde_json::json;
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_typed_decision_route_decides_over_the_unchanged_wire_and_is_accounted() {
        // Recorded reply in the published `/v1/systemone` shape (see
        // `systemone.rs`); no server has been reached from a kGraf route.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "jev-latest",
                "answers": {"q": {"type": "noul", "noul": 0.7}},
                "usage": {"input_tokens": 20, "output_tokens": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let decider: Arc<dyn Decider> =
            Arc::new(SystemOneClient::new(server.uri(), 8192).expect("client"));
        let route = ModelRoute::new(
            "systemone:jev-latest",
            KIND,
            systemone::BACKEND,
            Protocol::Inclusive,
            "jev-latest",
            decider,
        );
        assert!(
            route
                .handle::<Arc<dyn Decider>>(kgraf_core::ModelKind::GENERATIVE)
                .is_err(),
            "a typed-decision route never serves generative work"
        );
        let handle = route.handle::<Arc<dyn Decider>>(KIND).expect("decider");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(route.adapter(), route.protocol(), route.model(), "a", "t")
            .expect("begin");
        let answers = handle
            .decide(
                &State::new("s", "x"),
                &[Question::noul("q", "yes?")],
                &token,
            )
            .await
            .expect("decide");
        assert!(matches!(answers[0], Answer::Noul { p_yes, .. } if (p_yes - 0.7).abs() < 1e-9));
        let record = ledger.call(token.id()).expect("read").expect("row");
        assert_eq!(record.settlement, Settlement::Counted);
        assert_eq!(record.usage.uncached_input, 20);
    }
}
