//! Model kinds: what a route's model returns (decision 0089).
//!
//! kGraf takes models of every kind. A **generative** model returns prose and
//! tool calls; a **typed-decision** model returns a value inside a declared
//! answer space with a probability; an **embedding** model returns vectors; a
//! **rerank** model returns relevance scores over a document list. Every one of
//! them is a route: declared the same way, credentialed the same way, and
//! accounted through the same ledger.
//!
//! # The kind API (stable; adapters build against it)
//!
//! - [`ModelKind`] is an open, `Copy` name, not a closed enum. The kinds kGraf
//!   ships are the constants [`ModelKind::GENERATIVE`],
//!   [`ModelKind::TYPED_DECISION`], [`ModelKind::EMBEDDING`] and
//!   [`ModelKind::RERANK`].
//! - [`KINDS`] is the registry: one [`KindSpec`] row per kind, with the name a
//!   user types, a one-line summary for the planner, and the tools that reach
//!   it. [`ModelKind::parse`] accepts only a registered name (fail loud).
//! - [`split_kind_prefix`] reads the `kind=route:model` spelling every surface
//!   (the command line, `run.json`, the desktop list) uses. No prefix means
//!   generative, so every route declared before kinds existed keeps its meaning.
//! - [`KindError`] is the one error for a kind that is unknown, missing, or
//!   used where another kind was needed. Routing never sends generative work to
//!   a non-generative route or the reverse: the mismatch raises.
//!
//! **An adapter for a new vendor on an existing kind** (a new generative wire,
//! for example) needs nothing here: the catalog's generative builder already
//! covers every wire that is not claimed by another kind.
//!
//! **A new kind** (a world model, for example) is additive rows plus a trait,
//! a tool and a method: one [`KindSpec`] row and one constant here; a call
//! trait for it (beside the others in `kgraf-provider`'s `kinds.rs`, or in its
//! own crate); in `kgraf-fleet`, a builder row in `KIND_BUILDERS` (and the
//! function it names, which builds the adapter) and the kind added to the
//! `KIND_CLAIMS` rows of the catalog routes that serve it; in `kgraf-tools`, a
//! tool factory row in `model_tools::MODEL_TOOLS`, the tool it builds, and a
//! typed method on `ModelCalls` that reaches the route through
//! `ModelCalls::open`, the one accounted call path. No existing match over
//! kinds has to change: kinds are found by table lookup. The adapter, the tool
//! and the method are new code the kind needs.
//!
//! Existing crates were considered first: crates.io offers clients for
//! "rerank", "embeddings client" and "openai embeddings" (`async-openai`,
//! `openai-api-rs`, `rig-core`), which tie a kind to their own client and usage
//! types, which decision 0006 rules out for the provider seam; `fastembed` runs
//! models locally. None offers an open kind registry, so this small one is new
//! code.

use std::fmt;

/// What a route's model returns. Open: a name from [`KINDS`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModelKind(&'static str);

impl ModelKind {
    /// Prose and tool calls: the chat, responses and messages routes. The
    /// default for a route declared without a kind.
    pub const GENERATIVE: ModelKind = ModelKind("generative");
    /// A value inside a declared answer space, with a probability (the
    /// `/v1/systemone` wire, decision 0042).
    pub const TYPED_DECISION: ModelKind = ModelKind("typed-decision");
    /// One vector per input text (`/v1/embeddings`).
    pub const EMBEDDING: ModelKind = ModelKind("embedding");
    /// Relevance scores of documents against a query (`/v1/rerank`).
    pub const RERANK: ModelKind = ModelKind("rerank");

    /// The kind with this registered name, or [`KindError::Unknown`].
    pub fn parse(name: &str) -> Result<ModelKind, KindError> {
        KINDS
            .iter()
            .find(|spec| spec.kind.0 == name.trim())
            .map(|spec| spec.kind)
            .ok_or_else(|| KindError::Unknown {
                name: name.to_owned(),
                known: known_names(),
            })
    }

    /// The kind's name, as a user types it.
    pub fn as_str(self) -> &'static str {
        self.0
    }

    /// The kind's registry row.
    ///
    /// A value from [`Self::parse`] always has one. A constant declared without
    /// its [`KINDS`] row is a defect a test catches; should one ship, it raises
    /// [`KindError::Unknown`] here rather than borrowing another kind's row.
    pub fn spec(self) -> Result<&'static KindSpec, KindError> {
        KINDS
            .iter()
            .find(|spec| spec.kind == self)
            .ok_or_else(|| KindError::Unknown {
                name: self.0.to_owned(),
                known: known_names(),
            })
    }

    /// Whether this is the generative kind.
    pub fn is_generative(self) -> bool {
        self == Self::GENERATIVE
    }

    /// The call kind a ledger row of this model kind records (`call_kind`):
    /// the kind's first tool name, or the kind's own name when it has none.
    /// One function, so the row writer and every reader that tells a
    /// non-generative row from a generative one agree.
    pub fn call_kind(self) -> &'static str {
        KINDS
            .iter()
            .find(|spec| spec.kind == self)
            .and_then(|spec| spec.tools.first().copied())
            .unwrap_or(self.0)
    }

    /// `Ok` when `self` is `expected`, else [`KindError::Mismatch`] naming
    /// `route`. The one check every caller that routes work uses.
    pub fn require(self, expected: ModelKind, route: &str) -> Result<(), KindError> {
        if self == expected {
            return Ok(());
        }
        Err(KindError::Mismatch {
            route: route.to_owned(),
            expected,
            actual: self,
        })
    }
}

impl Default for ModelKind {
    fn default() -> Self {
        Self::GENERATIVE
    }
}

impl fmt::Debug for ModelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ModelKind({})", self.0)
    }
}

impl fmt::Display for ModelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::str::FromStr for ModelKind {
    type Err = KindError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl serde::Serialize for ModelKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for ModelKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        ModelKind::parse(&name).map_err(serde::de::Error::custom)
    }
}

/// One registered kind.
#[derive(Clone, Copy, Debug)]
pub struct KindSpec {
    pub kind: ModelKind,
    /// What the kind returns, in one line, for the planner's context.
    pub summary: &'static str,
    /// The tools that reach a route of this kind. Empty for generative: a
    /// generative route is a worker or the planner, not a tool.
    pub tools: &'static [&'static str],
}

/// The registry. The first row is the default kind.
pub const KINDS: &[KindSpec] = &[
    KindSpec {
        kind: ModelKind::GENERATIVE,
        summary: "prose and tool calls; runs workers and the planner",
        tools: &[],
    },
    KindSpec {
        kind: ModelKind::TYPED_DECISION,
        summary: "a typed question about supplied state answered inside a declared answer \
                  space (yes/no, pick one, score) with a probability",
        tools: &["decide"],
    },
    KindSpec {
        kind: ModelKind::EMBEDDING,
        summary: "one vector per text; ranks texts against a query by cosine similarity",
        tools: &["embed"],
    },
    KindSpec {
        kind: ModelKind::RERANK,
        summary: "relevance scores of documents against a query, best first",
        tools: &["rerank"],
    },
];

/// Whether a ledger row's `call_kind` names a non-generative model call
/// ([`ModelKind::call_kind`] of a kind other than generative). Such a row is
/// one embedding, rerank or decision request, not a round of a worker or the
/// planner, so round statistics leave it out.
pub fn is_non_generative_call_kind(call_kind: &str) -> bool {
    KINDS
        .iter()
        .filter(|spec| !spec.kind.is_generative())
        .any(|spec| spec.kind.call_kind() == call_kind)
}

fn known_names() -> String {
    KINDS
        .iter()
        .map(|spec| spec.kind.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Split an optional `kind=` prefix off a route spec.
///
/// `embedding=openai:text-embedding-3-small` is `(EMBEDDING, "openai:text-embedding-3-small")`;
/// `anthropic:claude-x` is `(GENERATIVE, "anthropic:claude-x")`. The prefix is
/// recognized only before the first `:`, since a route id never holds `=`
/// and a model name may. An unregistered prefix raises rather than being read
/// as part of the route id.
pub fn split_kind_prefix(spec: &str) -> Result<(ModelKind, &str), KindError> {
    let colon = spec.find(':').unwrap_or(spec.len());
    match spec.find('=') {
        Some(equals) if equals < colon => {
            let kind = ModelKind::parse(&spec[..equals])?;
            Ok((kind, &spec[equals + 1..]))
        }
        _ => Ok((ModelKind::GENERATIVE, spec)),
    }
}

/// A kind that is unknown, absent, or not the one the caller needs.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KindError {
    /// A kind name no [`KindSpec`] registers.
    #[error("unknown model kind `{name}`; known: {known}")]
    Unknown { name: String, known: String },
    /// The route serves another kind than the work needs. Raised, never
    /// rerouted: generative work never reaches a non-generative route or the
    /// reverse.
    #[error("route `{route}` serves {actual} models, not {expected}")]
    Mismatch {
        route: String,
        expected: ModelKind,
        actual: ModelKind,
    },
    /// No configured route serves this kind.
    #[error("no {kind} route is configured; declare one as `{kind}=route:model`")]
    NoRoute { kind: ModelKind },
    /// The named route is not configured at all.
    #[error("route `{route}` is not configured; the {kind} routes are: {known}")]
    UnknownRoute {
        route: String,
        kind: ModelKind,
        known: String,
    },
    /// The catalog route cannot serve this kind (no such endpoint on its wire).
    #[error("route `{route}` does not serve {kind} models; routes that do: {known}")]
    NotServed {
        route: String,
        kind: ModelKind,
        known: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_constant_has_a_registry_row_and_names_are_unique() {
        for kind in [
            ModelKind::GENERATIVE,
            ModelKind::TYPED_DECISION,
            ModelKind::EMBEDDING,
            ModelKind::RERANK,
        ] {
            assert_eq!(kind.spec().expect("row").kind, kind);
            assert_eq!(ModelKind::parse(kind.as_str()).expect("parse"), kind);
        }
        let mut names: Vec<_> = KINDS.iter().map(|s| s.kind.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KINDS.len());
        assert_eq!(KINDS[0].kind, ModelKind::default());
    }

    #[test]
    fn a_kind_without_a_registry_row_raises_instead_of_borrowing_another_row() {
        // Only this module can build such a value: it is what a constant
        // declared without its row would be.
        let orphan = ModelKind("world");
        let err = orphan.spec().expect_err("no row");
        assert!(matches!(err, KindError::Unknown { .. }), "{err}");
    }

    #[test]
    fn an_unknown_kind_raises_and_names_the_known_ones() {
        let err = ModelKind::parse("world").expect_err("unknown");
        assert!(err.to_string().contains("embedding"), "{err}");
    }

    #[test]
    fn a_spec_without_a_prefix_is_generative() {
        assert_eq!(
            split_kind_prefix("anthropic:claude-x").expect("split"),
            (ModelKind::GENERATIVE, "anthropic:claude-x")
        );
    }

    #[test]
    fn a_prefix_names_the_kind_and_the_rest_is_the_route() {
        assert_eq!(
            split_kind_prefix("embedding=openai:text-embedding-3-small").expect("split"),
            (ModelKind::EMBEDDING, "openai:text-embedding-3-small")
        );
        assert_eq!(
            split_kind_prefix("typed-decision=systemone:jev-latest").expect("split"),
            (ModelKind::TYPED_DECISION, "systemone:jev-latest")
        );
    }

    #[test]
    fn an_equals_sign_inside_the_model_name_is_not_a_prefix() {
        assert_eq!(
            split_kind_prefix("ollama:model=q4").expect("split"),
            (ModelKind::GENERATIVE, "ollama:model=q4")
        );
    }

    #[test]
    fn an_unregistered_prefix_raises_rather_than_joining_the_route_id() {
        assert!(matches!(
            split_kind_prefix("world=openai:m"),
            Err(KindError::Unknown { .. })
        ));
    }

    #[test]
    fn require_raises_on_a_mismatch_naming_both_kinds() {
        ModelKind::EMBEDDING
            .require(ModelKind::EMBEDDING, "openai:e")
            .expect("same kind");
        let err = ModelKind::EMBEDDING
            .require(ModelKind::GENERATIVE, "openai:e")
            .expect_err("mismatch");
        let text = err.to_string();
        assert!(
            text.contains("embedding") && text.contains("generative"),
            "{text}"
        );
    }

    #[test]
    fn a_kind_round_trips_through_json_and_an_unknown_one_is_refused() {
        let json = serde_json::to_string(&ModelKind::RERANK).expect("ser");
        assert_eq!(json, "\"rerank\"");
        let back: ModelKind = serde_json::from_str(&json).expect("de");
        assert_eq!(back, ModelKind::RERANK);
        assert!(serde_json::from_str::<ModelKind>("\"world\"").is_err());
    }

    #[test]
    fn the_call_kind_of_each_non_generative_kind_is_its_tool_and_is_recognized() {
        assert_eq!(ModelKind::EMBEDDING.call_kind(), "embed");
        assert_eq!(ModelKind::RERANK.call_kind(), "rerank");
        assert_eq!(ModelKind::TYPED_DECISION.call_kind(), "decide");
        for spec in KINDS {
            assert_eq!(
                is_non_generative_call_kind(spec.kind.call_kind()),
                !spec.kind.is_generative(),
                "{spec:?}"
            );
        }
        // The call kinds a worker round and the planner record.
        for generative in ["worker_round", "decompose", "reground", "generative"] {
            assert!(!is_non_generative_call_kind(generative), "{generative}");
        }
    }

    #[test]
    fn only_non_generative_kinds_name_tools() {
        for spec in KINDS {
            assert_eq!(spec.tools.is_empty(), spec.kind.is_generative(), "{spec:?}");
        }
    }
}
