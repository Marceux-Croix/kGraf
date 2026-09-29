//! The worker as the command line runs it, assembled from the merged
//! pieces: `kgraf-tools`' built-ins sandboxed through `kgraf-sandbox`'s
//! `Workspace`, `kgraf-session`'s durable transcript and masking, MCP servers
//! from the configuration with every exchange in the ledger, and the per-call
//! trace fields the runtime writes through `Ledger::begin_traced`.
//!
//! A library module rather than code in the binary so the end-to-end test drives
//! exactly what `kgraf worker` runs, with only the provider swapped. It lives
//! in `kgraf-host` because every host (this command, the desktop shell)
//! assembles the orchestrator's attempt environments through it;
//! `kgraf-cli` re-exports it as `kgraf_cli::worker`.

pub mod ask;
pub mod assemble;
pub mod config;
pub mod run;
pub mod segment;
pub mod workspace;
