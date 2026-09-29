//! Identifier newtypes.
//!
//! Ids are strings rather than integers because several of them are
//! *deterministic*: a graph node id is derived from the node's own identity so
//! that a reindex produces the same id for the same element (decision 0007,
//! and the stable-layout requirement), and a delegation session id
//! is derived from sender, target and root so it can be used directly as a
//! graph edge id (decision 0005).

use std::fmt;

macro_rules! id_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap an already-formed identifier.
            pub fn new(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            /// The identifier as it is stored.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), &self.0)
            }
        }

        impl From<String> for $name {
            fn from(raw: String) -> Self {
                Self(raw)
            }
        }

        impl From<&str> for $name {
            fn from(raw: &str) -> Self {
                Self(raw.to_owned())
            }
        }
    };
}

id_newtype! {
    /// A file in the indexed working tree.
    FileId
}

id_newtype! {
    /// A code element: the unit the graph draws and the unit an agent reads.
    NodeId
}

id_newtype! {
    /// One addressable, persisted unit of an agent's work (decision 0007).
    RunId
}

id_newtype! {
    /// A live agent. Distinct from `RunId`: one agent may own several runs.
    AgentId
}

id_newtype! {
    /// A unit of planned work the orchestrator hands to a worker.
    TaskId
}

id_newtype! {
    /// One accounted provider request.
    CallId
}

/// Derive a stable identifier from parts, so the same input always yields the
/// same id across processes and across reindexes.
///
/// The parts are length-prefixed before hashing so that `["ab", "c"]` and
/// `["a", "bc"]` cannot collide.
pub fn stable_id(domain: &str, parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"\x00");
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let hash = hasher.finalize();
    // 16 hex chars is 64 bits, which is ample for a single project's index and
    // keeps the ids readable in a debug dump.
    hash.to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_id_is_deterministic() {
        let a = stable_id("node", &["src/lib.rs", "function", "parse"]);
        let b = stable_id("node", &["src/lib.rs", "function", "parse"]);
        assert_eq!(a, b);
    }

    #[test]
    fn stable_id_is_length_prefixed_so_parts_cannot_run_together() {
        let a = stable_id("node", &["ab", "c"]);
        let b = stable_id("node", &["a", "bc"]);
        assert_ne!(a, b);
    }

    #[test]
    fn domains_are_separated() {
        assert_ne!(stable_id("node", &["x"]), stable_id("edge", &["x"]));
    }

    #[test]
    fn ids_round_trip_through_json() {
        let id = NodeId::new("abc123");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"abc123\"");
        let back: NodeId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }
}
