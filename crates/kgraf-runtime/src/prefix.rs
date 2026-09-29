//! The shared prefix and its digest (decision 0043).
//!
//! Provider prompt caches key on an exact token prefix, so every worker on the same
//! vendor and model must send **byte-identical** bytes up to the cache breakpoint.
//! [`SharedPrefix`] is everything in that region:
//!
//! - the system prompt blocks,
//! - the tool definitions (`ToolRegistry::prefix_definitions`: never filtered per
//!   worker, deferred tools excluded),
//! - the skill catalog (names and descriptions; bodies go in the private remainder),
//! - the slice pool (node ids and the rendered text sent for them),
//! - the MCP servers' instructions.
//!
//! **One snapshot per vendor and model.** All workers dispatched to one vendor and
//! model share one `ToolRegistry` snapshot and one `SharedPrefix` value. A change to
//! any part (a new MCP tool list, a new skill) makes a new snapshot, applied at the
//! next dispatch, never mid-run.
//!
//! **The digest is over the bytes the adapter sends.** Two workers are proven to
//! share a prefix only by the bytes that go on the wire; a digest over
//! `serde_json::Value::to_string`, or any other encoding the adapter does not use,
//! could match while the requests differ (key order, whitespace, a field the
//! adapter adds). So the digest takes a [`PrefixEncoder`], implemented by each
//! adapter with the same serialization it uses for the request, and hashes the
//! adapter's name and exactly those bytes. The acceptance test compares these
//! digests across workers and records the digest in each call's trace.

use kgraf_core::NodeId;
use kgraf_provider::message::{Content, ToolDefinition};
use kgraf_session::skills::CatalogEntry;
use serde::Serialize;

/// One slice in the shared pool: its node and the text sent for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PrefixSlice {
    pub node: NodeId,
    pub text: String,
}

/// One MCP server's instructions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct McpInstructions {
    pub server: String,
    pub text: String,
}

/// Everything before the cache breakpoint.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SharedPrefix {
    pub vendor: String,
    pub model: String,
    pub system: Vec<Content>,
    pub tools: Vec<ToolDefinition>,
    pub skill_catalog: Vec<CatalogEntry>,
    pub slice_pool: Vec<PrefixSlice>,
    pub mcp_instructions: Vec<McpInstructions>,
}

/// A prefix could not be encoded.
#[derive(Debug, thiserror::Error)]
#[error("adapter `{adapter}` cannot encode the shared prefix: {reason}")]
pub struct PrefixError {
    pub adapter: String,
    pub reason: String,
}

/// Encodes a prefix exactly as an adapter sends it. Implemented in each adapter
/// crate with the serialization its request path uses.
pub trait PrefixEncoder: Send + Sync {
    /// The adapter's stable name.
    fn adapter(&self) -> &'static str;

    /// The request bytes up to the cache breakpoint.
    fn encode_prefix(&self, prefix: &SharedPrefix) -> Result<Vec<u8>, PrefixError>;
}

/// blake3 over an adapter's encoded prefix bytes, hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct PrefixDigest(String);

impl PrefixDigest {
    /// The digest, hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SharedPrefix {
    /// The digest of the bytes `encoder` sends for this prefix.
    pub fn digest(&self, encoder: &dyn PrefixEncoder) -> Result<PrefixDigest, PrefixError> {
        let bytes = encoder.encode_prefix(self)?;
        let mut h = blake3::Hasher::new();
        let adapter = encoder.adapter().as_bytes();
        h.update(&(adapter.len() as u64).to_le_bytes());
        h.update(adapter);
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
        Ok(PrefixDigest(h.finalize().to_hex().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Encodes with serde_json, as a JSON-body adapter would.
    struct JsonEncoder;

    impl PrefixEncoder for JsonEncoder {
        fn adapter(&self) -> &'static str {
            "json-test"
        }
        fn encode_prefix(&self, prefix: &SharedPrefix) -> Result<Vec<u8>, PrefixError> {
            serde_json::to_vec(prefix).map_err(|e| PrefixError {
                adapter: "json-test".into(),
                reason: e.to_string(),
            })
        }
    }

    /// Same content, pretty-printed: different wire bytes.
    struct PrettyEncoder;

    impl PrefixEncoder for PrettyEncoder {
        fn adapter(&self) -> &'static str {
            "json-test"
        }
        fn encode_prefix(&self, prefix: &SharedPrefix) -> Result<Vec<u8>, PrefixError> {
            serde_json::to_vec_pretty(prefix).map_err(|e| PrefixError {
                adapter: "json-test".into(),
                reason: e.to_string(),
            })
        }
    }

    fn prefix() -> SharedPrefix {
        SharedPrefix {
            vendor: "v".into(),
            model: "m".into(),
            system: vec![Content::cached_text("You are a worker.")],
            tools: vec![ToolDefinition {
                name: "edit".into(),
                description: "Edit a file".into(),
                input_schema: json!({"type": "object"}),
            }],
            skill_catalog: vec![CatalogEntry {
                name: "review".into(),
                description: "Review a diff".into(),
            }],
            slice_pool: vec![PrefixSlice {
                node: NodeId::new("n1"),
                text: "fn main() {}".into(),
            }],
            mcp_instructions: vec![McpInstructions {
                server: "github".into(),
                text: "Prefer search over list.".into(),
            }],
        }
    }

    #[test]
    fn two_workers_with_one_snapshot_get_one_digest() {
        let a = prefix().digest(&JsonEncoder).expect("a");
        let b = prefix().digest(&JsonEncoder).expect("b");
        assert_eq!(a, b);
    }

    #[test]
    fn every_part_of_the_prefix_reaches_the_digest() {
        let base = prefix().digest(&JsonEncoder).expect("base");
        let changes: Vec<fn(&mut SharedPrefix)> = vec![
            |p| p.system.push(Content::text("x")),
            |p| p.tools[0].description.push('!'),
            |p| p.skill_catalog[0].description.push('!'),
            |p| p.slice_pool[0].text.push(' '),
            |p| p.slice_pool[0].node = NodeId::new("n2"),
            |p| p.mcp_instructions[0].text.push('!'),
            |p| p.model.push('2'),
        ];
        for (i, change) in changes.into_iter().enumerate() {
            let mut p = prefix();
            change(&mut p);
            assert_ne!(p.digest(&JsonEncoder).expect("d"), base, "change {i}");
        }
    }

    #[test]
    fn the_digest_follows_the_adapters_bytes_not_the_content() {
        // Same content, different wire bytes: a digest over anything but the
        // adapter's encoding would call these the same prefix.
        assert_ne!(
            prefix().digest(&JsonEncoder).expect("compact"),
            prefix().digest(&PrettyEncoder).expect("pretty")
        );
    }
}
