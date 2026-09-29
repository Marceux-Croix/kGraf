//! The interpretation pass: a corpus, in any format, into a plan
//! model of planned components and work items with dependencies and acceptance
//! criteria, which the user approves and which then seeds the graph and the
//! orchestrator.
//!
//! ```text
//!   prepare  gather: kgraf-ingest documents + incremental graph index
//!            rank:   aider repo-map PageRank over the graph's references (port)
//!            brief:  bounded slices, pinned instruction, omissions listed
//!   ask      one model call through the caller's ledgered Asker, or none when
//!            this brief was read by this model before
//!   finish   parse, validate, assess commands and criteria, reconcile doneness
//!            against code and tests, name the first dispatchable item, record
//!            the reading under its plan id
//!   approve  the user's agreement, bound to the plan id they were shown; no
//!            model call; proposed commands only by name; complete items are
//!            not re-seeded
//!   seed     work nodes in the graph; the orchestrator's plan starts from them
//! ```
//!
//! Three acceptance modes come out of the same pass ([`Mode`]): **greenfield** (an
//! empty directory and an instruction: scoping questions and a first plan),
//! **existing** (documentation, no planned work in the code yet) and **resume**
//! (some planned work already in the code: it derives Complete or Partial from code
//! and tests and is not redone).
//!
//! **Reused (reuse existing code).** Parsing and chunking: `kgraf-ingest` (pulldown-cmark,
//! pdf-extract, docx-rs, text-splitter). Ranking: aider's repo-map PageRank, ported
//! into `kgraf_graph::rank` (Apache-2.0, `third_party/aider/`). Structural
//! matching with `ast-grep-core` was evaluated and not needed here: the graph's
//! tree-sitter index already answers the one structural question this pass asks
//! (does a named symbol exist). The pass itself, the brief, the plan schema and the
//! reconciliation are kGraf's own (the roadmap: "the interpretation pass that
//! turns a corpus into planned components is kGraf's own").
//!
//! **Constraints demonstrably bind (decision 0013).** A constraint may name
//! `forbidden_paths`; [`approve::Interpretation::approve`] strikes any work item's
//! declared write that falls under one before the plan is seeded, so a worker is
//! never contracted to write there, and the existing write-scope check refuses
//! any attempt outside its declared writes regardless of what the model does.
//! [`ApprovedPlan::blocked_writes`] names what was struck. A constraint that names
//! no path (most of them: "no kanban" forbids a behavior, not a location) still
//! reaches every worker's instruction text, but nothing mechanical enforces it;
//! that is the pass's stated limit, not a silent one.

pub mod approve;
pub mod brief;
pub mod corpus;
pub mod error;
pub mod model;
pub mod pass;
pub mod presumed;
pub mod reconcile;
pub mod roadmap;

pub use approve::{ApprovedPlan, BlockedWrite, SeedItem, Seeded, approve_stored};
pub use brief::{BriefStats, DEFAULT_INPUT_BUDGET, MIN_INPUT_BUDGET};
pub use corpus::GatherOptions;
pub use error::InterpretError;
pub use model::{
    Answer, AssessedCommand, Check, Claim, CommandProvenance, Component, Constraint, Criterion,
    DeclaredAs, Evidence, PlanLimits, ValidationCommand, Verifiability, WorkItem,
};
pub use pass::{
    Answered, AskAnswer, AskFailure, Asker, InterpretRequest, Interpretation, KEEP_READINGS, Mode,
    Prepared, Timings, Unverifiable, prepare,
};
pub use presumed::{PresumedState, PresumedView, ReadingReport, SourceReport};
pub use reconcile::{
    Discrepancy, Doneness, ItemState, NoTestRuns, RecordedTests, TestOutcome, TestResults,
};
