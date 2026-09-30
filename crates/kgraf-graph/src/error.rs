//! Library errors.
//!
//! `thiserror` in the library, `anyhow` only at the binary boundary, per the
//! project's code conventions.

/// Errors the graph can produce.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// The embedded database refused an operation.
    #[error("graph database error")]
    Sqlite(#[from] rusqlite::Error),

    /// A file could not be read or walked.
    #[error("cannot read `{path}`")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A tree-sitter grammar could not be installed on the parser. This is a
    /// build-time mismatch between the grammar crate and the tree-sitter
    /// runtime, not a user error, and it raises rather than degrading to a
    /// text-only index (fail loud).
    #[error("cannot load the {language} grammar")]
    Grammar {
        language: &'static str,
        #[source]
        source: tree_sitter::LanguageError,
    },

    /// A grammar crate's `TAGS_QUERY` does not compile against its own grammar. A
    /// build-time mismatch like [`GraphError::Grammar`], raised rather than
    /// indexing the language's files as opaque (fail loud).
    #[error("the {language} tags query does not compile: {reason}")]
    TagsQuery {
        language: &'static str,
        reason: String,
    },

    /// A file's bytes are not valid UTF-8, so it cannot be parsed. Binary files
    /// are skipped before this point; reaching it means a text extension held
    /// something else.
    #[error("`{path}` is not valid UTF-8")]
    NotUtf8 { path: String },

    /// A symbol lookup found nothing.
    #[error("no node named `{name}` in the graph")]
    UnknownSymbol { name: String },

    /// A structural query could not be parsed.
    #[error("cannot parse query `{query}`: {reason}")]
    BadQuery { query: String, reason: String },

    /// A node's recorded span no longer lies inside its file. This is how a
    /// stale index announces itself instead of serving the wrong lines.
    #[error(
        "node `{node}` spans lines {start}-{end} but `{path}` has {lines} lines; reindex needed"
    )]
    StaleSpan {
        node: String,
        path: String,
        start: u32,
        end: u32,
        lines: u32,
    },

    /// The layout could not be derived from the graph (for example, the layout
    /// cache is out of step with the nodes).
    #[error("cannot lay out the graph")]
    Layout(#[from] crate::layout::LayoutError),

    /// Version history could not be read.
    #[error("cannot read version history: {reason}")]
    History { reason: String },

    /// A sub-file lease's node id no longer exists in the graph.
    ///
    /// Decision 0050: a vanished id is never carried to a "nearest" element, because
    /// that would be a silent substitution (fail loud). The holder re-acquires against the
    /// current snapshot instead. This is distinct from an ordinary lapse: expiry claims
    /// only that a lease is *stale*, while this claims the thing it named is *gone*.
    #[error("node `{node_id}` no longer exists; its lease is invalid, not renewable")]
    VanishedLeaseTarget { node_id: String },

    /// A stored kind name this build does not know. Raised rather than mapped to a
    /// default kind, which would draw the row with the wrong size (fail loud).
    #[error("unknown {what} kind `{value}` in the index")]
    UnknownKind { what: &'static str, value: String },

    /// The repo-map PageRank did not converge within its iteration cap. Raised as
    /// networkx raises `PowerIterationFailedConvergence`, never returned as a
    /// half-iterated ranking (fail loud).
    #[error("the repository ranking did not converge in {iterations} iterations")]
    RankDidNotConverge { iterations: usize },

    /// A personalization vector whose length is not the node count.
    #[error("a ranking personalization of {given} values was given for {nodes} nodes")]
    RankPersonalization { nodes: usize, given: usize },

    /// A work node or dependency could not be stored as given.
    #[error("invalid work node: {reason}")]
    InvalidWork { reason: String },

    /// A fact could not be asserted as asked (`facts.rs`).
    #[error("fact {id}: {reason}")]
    Fact { id: i64, reason: String },

    /// A read or facts open named an index file that does not exist. Only an index
    /// command creates one.
    #[error("no index at `{path}`; run `kgraf index` first")]
    NoIndex { path: String },

    /// A symbol name matches more than one node and none of them is an exact
    /// qualified-name match. `candidates` lists them as `qualified_name (path:line)`.
    #[error("`{name}` is ambiguous ({} candidates): {}", candidates.len(), candidates.join("; "))]
    AmbiguousSymbol {
        name: String,
        candidates: Vec<String>,
    },

    /// A node's file changed after it was indexed, so its recorded lines may
    /// name other text now.
    #[error("`{path}` changed since it was indexed (node `{node}`); run `kgraf index`")]
    StaleFile { node: String, path: String },

    /// An existing index was opened to record facts
    /// ([`crate::GraphStore::open_existing_for_facts`]) and its schema version is not
    /// this build's. Writing would first need a rebuild of the derived data, and only
    /// an index command may start one.
    #[error(
        "the index at `{path}` has schema version {found:?}; this build writes version \
         {expected}; run `kgraf index` to rebuild it"
    )]
    SchemaMismatch {
        path: String,
        found: Option<i64>,
        expected: i64,
    },
}

/// The graph's result alias.
pub type Result<T> = std::result::Result<T, GraphError>;

impl kgraf_diag::DiagCode for GraphError {
    fn diag_code(&self) -> &'static str {
        match self {
            GraphError::Io { .. } | GraphError::NotUtf8 { .. } => "KG-0201",
            GraphError::UnknownSymbol { .. } => "KG-0202",
            GraphError::BadQuery { .. } => "KG-0203",
            GraphError::SchemaMismatch { .. } => "KG-0204",
            GraphError::NoIndex { .. } => "KG-0205",
            GraphError::AmbiguousSymbol { .. } => "KG-0206",
            GraphError::StaleFile { .. } => "KG-0207",
            _ => kgraf_diag::catch_all_for("kgraf-graph").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-graph"
    }
}

#[cfg(test)]
mod diag_code_tests {
    use super::*;
    use kgraf_diag::DiagCode;

    /// Every `diag_code` branch resolves to a real glossary entry: changing `UnknownSymbol`'s code
    /// to `KG-9876` passed every other test in this crate before this one existed. One instance per
    /// branch of the `match` in `diag_code`, including the wildcard (`Sqlite`, which is not named
    /// there): not literally every variant, since the other unnamed ones share that same catch-all
    /// branch and would prove nothing additional.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        let instances: Vec<GraphError> = vec![
            GraphError::Io {
                path: "x".into(),
                source: std::io::Error::other("x"),
            },
            GraphError::NotUtf8 { path: "x".into() },
            GraphError::UnknownSymbol { name: "x".into() },
            GraphError::BadQuery {
                query: "x".into(),
                reason: "x".into(),
            },
            GraphError::SchemaMismatch {
                path: "x".into(),
                found: None,
                expected: 1,
            },
            GraphError::NoIndex { path: "x".into() },
            GraphError::AmbiguousSymbol {
                name: "x".into(),
                candidates: vec!["a".into(), "b".into()],
            },
            GraphError::StaleFile {
                node: "x".into(),
                path: "x".into(),
            },
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            GraphError::InvalidWork { reason: "x".into() },
        ];
        for e in instances {
            let code = e.diag_code();
            if !matches!(e, GraphError::InvalidWork { .. }) {
                assert_ne!(
                    code,
                    kgraf_diag::catch_all_for("kgraf-graph").code,
                    "{e:?} falls to the catch-all"
                );
            }
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }
}
