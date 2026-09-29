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
pub use decision::{Answer, AnswerSpace, Confidence, Question, State};
pub use fake::FakeDecider;
pub use install::{InstallChoice, InstallState, ModelManifest};
pub use systemone::SystemOneClient;
