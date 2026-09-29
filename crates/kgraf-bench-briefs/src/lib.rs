//! A per-model brief-following bench (item 8 of the coordination-hardening
//! set).
//!
//! **What this measures.** A fixed suite of small, self-contained briefs
//! (`briefs::suite`), at least two per [`kgraf_core::model_profile::TaskKind`],
//! each with a tiny fixture repository under `fixtures/` and a mechanical
//! check of whether the brief was satisfied: a `cargo test` run that must
//! pass, a script whose output must match, or a doc file whose required facts
//! and word cap the check reads directly (never a model judge). The runner
//! (`runner::run_bench`) drives one worker through kGraf's normal worker path
//! (`kgraf_host::worker`: the same assembly and `run_segment` `kgraf worker`
//! uses) per route and per brief, checks the result, and records
//! `(model, vendor, kind, satisfied, cost)` into `kgraf_core::model_profile`'s
//! `ProfileStore` with `Source::Bench`.
//!
//! **What this does not measure.** These are small, single-file tasks against
//! a fresh git repository with no MCP servers, indexed as a real run indexes
//! a repository, under `kgraf worker`'s default budget: a prior on a model's
//! brief-following, not a repo-scale benchmark. The router (decision 0075) treats a bench tally as a prior; a
//! live, verified `Source::Live` tally for the same model and kind, once one
//! exists, overrides it.
//!
//! Reuse search (decision 0055) before writing this crate: the project's own
//! evidence and `ROADMAP.md` reuse-candidate list have no existing
//! brief-following harness; crates.io has `criterion` (performance
//! benchmarking, not brief-following) and `insta` (snapshot testing, no
//! agent loop); GitHub has no permissively licensed harness that drives
//! kGraf's own worker/provider/ledger stack, since the whole point is
//! measuring *this* stack's own worker path (`kgraf_host::worker`,
//! `kgraf_runtime::spawn_worker`) rather than reimplementing a second agent
//! loop, which is why the runner calls straight into it instead.

pub mod brief;
pub mod check;
pub mod gitutil;
pub mod prices;
pub mod report;
pub mod runner;
pub mod structure;
pub mod suite_store;

pub use brief::{Brief, Check, DocFact, Mutation, ScriptCase, suite, suite_hash};
pub use prices::{
    apply_override, closest_keys, default_price_table, default_price_table_for, unpriced_warning,
};
pub use runner::{
    AttemptOutcome, BenchOptions, PlannedAttempt, RouteHandle, WorkerBudget, plan_attempts,
    run_bench,
};
