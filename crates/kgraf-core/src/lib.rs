//! Types shared by every kGraf crate.
//!
//! This crate is deliberately dependency-light and provider-agnostic. Nothing
//! in here may know the name of a model vendor: per decision 0006,
//! provider-specific types never escape their adapter crate.

pub mod budget;
pub mod cost;
pub mod flock;
pub mod ids;
pub mod legacy;
pub mod litellm;
pub mod model_profile;
pub mod palette;
pub mod parallel;
pub mod paths;
pub mod session;
pub mod usage;

pub use budget::{TokenBudget, estimate_tokens};
pub use cost::{CostError, ModelPrice, Money, PriceTable};
pub use ids::{AgentId, CallId, FileId, NodeId, RunId, TaskId};
pub use litellm::{
    LiteLlmCatalog, LoadError as LiteLlmLoadError, ModelInfo, UnmappedEntry, UnmappedReason,
};
pub use parallel::ParallelismLimits;
pub use usage::{Protocol, TokenUsage, UsageError};
