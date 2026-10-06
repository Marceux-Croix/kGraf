//! Typed interpretation errors. Each is raised rather than degraded into a guessed
//! or empty plan (fail loud).

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum InterpretError {
    #[error("reading the corpus: {0}")]
    Ingest(#[from] kgraf_ingest::IngestError),

    #[error("graph: {0}")]
    Graph(#[from] kgraf_graph::GraphError),

    /// Greenfield needs something to plan from: an empty directory and no
    /// instruction is nothing.
    #[error("{root} holds no documents and no code, and no instruction was given")]
    NothingToInterpret { root: PathBuf },

    /// The pinned text (instruction and corrections, verbatim) does not fit the
    /// input bound; it is never truncated.
    #[error(
        "the instruction and corrections are {tokens} tokens; at most {limit} of the {budget}-token input bound may be pinned"
    )]
    PinnedOverBudget {
        tokens: u64,
        limit: u64,
        budget: u64,
    },

    /// The input bound is too small to hold the fixed parts of a brief.
    #[error("an input bound of {budget} tokens is below the {minimum} tokens this brief needs")]
    BudgetTooSmall { budget: u64, minimum: u64 },

    /// The assembled brief measured over its bound. Never sent.
    #[error("the brief is {tokens} tokens, over its {budget}-token bound; it was not sent")]
    InputOverBudget { tokens: u64, budget: u64 },

    /// The request names no model; the reading is keyed by it.
    #[error("the interpretation request names no model")]
    NoModel,

    /// The asker is not the model the request (and so the cache key) names.
    #[error("the request names model `{expected}` but the asker is `{got}`")]
    ModelMismatch { expected: String, got: String },

    /// A panic elsewhere poisoned the graph store's lock.
    #[error("the graph store lock was poisoned by a panic")]
    Poisoned,

    /// The model call failed. `call` is the ledger row when one was opened, so the
    /// caller can settle it.
    #[error("the interpretation call failed: {message}")]
    Model {
        message: String,
        call: Option<String>,
        /// The model's output was malformed ([`AskFailure::malformed`]).
        ///
        /// [`AskFailure::malformed`]: crate::AskFailure::malformed
        malformed: bool,
    },

    /// The answer is not the JSON asked for.
    #[error(
        "the model's answer is not the plan JSON asked for: {reason}; answer begins {excerpt:?}"
    )]
    BadAnswer { reason: String, excerpt: String },

    /// The answer parsed, but the plan in it is not usable as given.
    #[error("the interpreted plan is invalid: {0}")]
    InvalidPlan(String),

    /// A plan with no work items cannot be approved.
    #[error("the plan has no work items to approve; answer its questions first: {questions:?}")]
    NothingToApprove { questions: Vec<String> },

    /// Approval names a plan other than this reading.
    #[error("approval names plan `{given}`, but this reading is plan `{expected}`")]
    PlanIdMismatch { expected: String, given: String },

    /// No stored reading has this plan id: nothing the user saw can be approved
    /// under it.
    #[error(
        "no stored reading is plan `{plan_id}`; read the corpus first and approve the id it prints"
    )]
    NoSuchReading { plan_id: String },

    /// The corpus changed between the reading and its approval: what the user
    /// read may no longer be what the documents say (a proposed command may have
    /// become declared, a requirement may have moved). Read again.
    #[error(
        "the corpus changed since plan `{plan_id}` was read; read it again and approve the new plan id"
    )]
    CorpusChanged { plan_id: String },

    /// A stored reading failed validation on load.
    #[error("the stored reading `{plan_id}` is not valid: {reason}")]
    BadStoredReading { plan_id: String, reason: String },

    /// No roadmap or plan document was found where they are looked for, and
    /// none was named (decision 0064). Said, never skipped in silence.
    #[error("no roadmap found: looked for {looked}; name one with --source")]
    NoRoadmap { looked: String },

    /// A named roadmap path is absolute, empty or leaves the project.
    #[error("`{path}` is not a path inside the project")]
    RoadmapPath { path: String },

    /// A roadmap document is larger than the parser reads (`MAX_ROADMAP_BYTES`).
    #[error("the roadmap `{path}` is {bytes} bytes, over the {limit}-byte limit; it was not read")]
    RoadmapTooLarge {
        path: String,
        bytes: u64,
        limit: u64,
    },

    /// A roadmap document could not be read.
    #[error("cannot read the roadmap `{path}`: {reason}")]
    RoadmapUnreadable { path: String, reason: String },

    /// A presumed node's stored body is not valid JSON. Reading it as empty
    /// would show the item as remaining work with no claim.
    #[error("the body of presumed node `{node}` is not valid JSON: {reason}")]
    CorruptBody { node: String, reason: String },

    /// Approval accepted a command the plan does not propose.
    #[error(
        "`{command}` is not a command this plan proposes; accept only proposed commands, by name"
    )]
    UnknownCommand { command: String },
}

impl InterpretError {
    /// Whether the model's output was malformed, so another sample of the
    /// same ask is worth making: the answer is not the JSON asked for (cut off
    /// mid-string, no object), or the server could not parse the model's
    /// output. A plan that parses but is not usable is not.
    pub fn is_malformed_answer(&self) -> bool {
        matches!(
            self,
            InterpretError::BadAnswer { .. }
                | InterpretError::Model {
                    malformed: true,
                    ..
                }
        )
    }
}

impl kgraf_diag::DiagCode for InterpretError {
    fn diag_code(&self) -> &'static str {
        match self {
            InterpretError::NothingToInterpret { .. } => "KG-1201",
            InterpretError::NoRoadmap { .. } => "KG-1202",
            _ => kgraf_diag::catch_all_for("kgraf-interpret").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-interpret"
    }
}

#[cfg(test)]
mod diag_code_tests {
    use super::*;
    use kgraf_diag::DiagCode;

    /// Every `diag_code` branch resolves to a real glossary entry. One instance per named branch, plus one
    /// unnamed variant for the catch-all.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        let instances: Vec<InterpretError> = vec![
            InterpretError::NothingToInterpret { root: "x".into() },
            InterpretError::NoRoadmap { looked: "x".into() },
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            InterpretError::NoModel,
        ];
        for e in &instances {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }
}
