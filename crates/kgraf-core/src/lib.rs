//! Types shared by every kGraf crate.
//!
//! This crate is deliberately dependency-light and provider-agnostic. Nothing
//! in here may know the name of a model vendor: per decision 0006,
//! provider-specific types never escape their adapter crate.

pub mod budget;
pub mod cost;
pub mod flock;
pub mod host_git;
pub mod ids;
pub mod legacy;
pub mod litellm;
pub mod model_kind;
pub mod model_profile;
pub mod output_limit;
pub mod palette;
pub mod parallel;
pub mod paths;
pub mod plain_path;
pub mod repo_lock;
pub mod session;
pub mod usage;

pub use budget::{TokenBudget, estimate_tokens};
pub use cost::{CostError, ExactMoney, ModelPrice, Money, PriceTable};
pub use ids::{AgentId, CallId, FileId, NodeId, RunId, TaskId};
pub use litellm::{
    LiteLlmCatalog, LoadError as LiteLlmLoadError, ModelInfo, UnmappedEntry, UnmappedReason,
};
pub use model_kind::{
    KINDS, KindError, KindSpec, ModelKind, is_non_generative_call_kind, split_kind_prefix,
};
pub use parallel::ParallelismLimits;
pub use usage::{Protocol, TokenUsage, UsageError};
