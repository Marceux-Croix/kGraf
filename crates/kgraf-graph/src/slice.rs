//! Context slices.
//!
//! This is the graph's most load-bearing job and the mechanism the rule that agents read graph
//! slices rests on: a requested symbol plus a bounded neighborhood, **sized to a token budget**. It
//! is also the only one of the project's token defenses with a measured figure behind it, at
//! roughly 23 percent (decision 0008).
//!
//! Two things are deliberate.
//!
//! **The slice reports what it left out.** A slice that silently truncates is
//! the retrieval failure that looks like a model failure, and the known weakness
//! of slice-based retrieval is cross-cutting changes, where recall drops exactly
//! when a change spans the codebase. So [`ContextSlice::omitted`] is part of the
//! result, and a caller that needs the whole file can see that it should ask.
//!
//! **The budget is spent in a fixed priority order**, not greedily by size: the
//! requested symbol first, then its enclosing element, then its callees, then
//! its callers, then its siblings. A budget large enough for the symbol alone
//! yields the symbol alone rather than three arbitrary neighbors.

use crate::error::{GraphError, Result};
use crate::store::{EdgeKind, GraphStore, NodeRecord};
use kgraf_core::budget::{TokenBudget, estimate_tokens};

/// Why a node is in a slice. Ordered by priority: lower comes first, and the
/// budget is spent in this order.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SliceReason {
    /// The symbol that was asked for.
    Requested,
    /// The element that contains the requested one, so the symbol is not read
    /// out of context.
    Enclosing,
    /// Something the requested element calls.
    Callee,
    /// Something that calls the requested element.
    Caller,
    /// A sibling under the same parent.
    Sibling,
}

impl SliceReason {
    /// The reason's stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            SliceReason::Requested => "requested",
            SliceReason::Enclosing => "enclosing",
            SliceReason::Callee => "callee",
            SliceReason::Caller => "caller",
            SliceReason::Sibling => "sibling",
        }
    }
}

/// One member of a slice.
#[derive(Clone, Debug)]
pub struct SliceMember {
    pub node: NodeRecord,
    pub reason: SliceReason,
    /// The source, read from disk at slice time.
    pub source: String,
    /// The estimated token cost this member charged to the budget.
    pub estimated_tokens: u64,
}

/// A bounded neighborhood of one symbol, ready to be put in front of a model.
#[derive(Clone, Debug)]
pub struct ContextSlice {
    /// The symbol that was asked for.
    pub requested: String,
    /// What made it into the budget, in priority order.
    pub members: Vec<SliceMember>,
    /// What was in the one-hop neighborhood but did not fit, with its reason.
    ///
    /// Reported rather than dropped: this is where a caller learns that the
    /// change it is making may be one of the cross-cutting ones slices are bad
    /// at, and that a justified whole-file read is the right fallback.
    pub omitted: Vec<(NodeRecord, SliceReason)>,
    /// Neighbors whose source could not be read, because the file changed after
    /// it was indexed or is gone, with the reason. Kept apart from
    /// [`ContextSlice::omitted`]: these did not fail the budget, they failed the
    /// read. A requested node that fails is an error, never listed here.
    pub unreadable: Vec<(NodeRecord, SliceReason, String)>,
    /// The budget after the slice was assembled.
    pub budget: TokenBudget,
}

impl ContextSlice {
    /// Estimated tokens the slice costs.
    pub fn estimated_tokens(&self) -> u64 {
        self.budget.spent()
    }

    /// Render the slice as the text a worker would receive.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for member in &self.members {
            // A tags-tier member says so: its nodes and edges come from the
            // grammar's upstream tags query, name-resolved (`lang.rs`, `Tier`).
            let fidelity = match crate::lang::Language::from_path(&member.node.path)
                .map(crate::lang::Language::tier)
            {
                Some(crate::lang::Tier::Tags) => " [tags tier: calls resolved by name only]",
                _ => "",
            };
            out.push_str(&format!(
                "// {} {}:{}-{} ({}){fidelity}\n",
                member.reason.as_str(),
                member.node.path,
                member.node.start_line,
                member.node.end_line,
                member.node.qualified_name
            ));
            out.push_str(&member.source);
            out.push_str("\n\n");
        }
        if !self.omitted.is_empty() {
            out.push_str(&format!(
                "// {} neighbor(s) omitted for budget: {}\n",
                self.omitted.len(),
                self.omitted
                    .iter()
                    .map(|(node, reason)| format!("{} ({})", node.qualified_name, reason.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.unreadable.is_empty() {
            out.push_str(&format!(
                "// {} neighbor(s) unreadable, not sliced: {}\n",
                self.unreadable.len(),
                self.unreadable
                    .iter()
                    .map(|(node, reason, why)| format!(
                        "{} ({}): {why}",
                        node.qualified_name,
                        reason.as_str()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out
    }
}

/// Resolve a symbol name to exactly one node. An exact qualified-name match wins
/// when it is the only one; otherwise more than one candidate is an error that
/// lists them all, never a silent pick of the first.
pub fn resolve_unambiguous(store: &GraphStore, symbol: &str) -> Result<NodeRecord> {
    let mut candidates = store.resolve_symbol(symbol)?;
    let exact: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, n)| n.qualified_name == symbol)
        .map(|(i, _)| i)
        .collect();
    if let [only] = exact.as_slice() {
        return Ok(candidates.swap_remove(*only));
    }
    if candidates.len() == 1 {
        return Ok(candidates.remove(0));
    }
    Err(GraphError::AmbiguousSymbol {
        name: symbol.to_owned(),
        candidates: candidates
            .iter()
            .map(|n| format!("{} ({}:{})", n.qualified_name, n.path, n.start_line))
            .collect(),
    })
}

/// Build a context slice for a symbol.
///
/// `budget_tokens` is an *estimate* ceiling, not a measurement; see
/// [`kgraf_core::budget`] for why it overcounts on purpose.
pub fn slice_for_symbol(
    store: &GraphStore,
    symbol: &str,
    budget_tokens: u64,
) -> Result<ContextSlice> {
    // An ambiguous name is refused with its candidates rather than resolved to
    // the first: the caller names the node it means (`slice_for_node`).
    let requested = resolve_unambiguous(store, symbol)?;
    slice_for_node(store, &requested, budget_tokens)
}

/// Build a context slice around a specific node.
pub fn slice_for_node(
    store: &GraphStore,
    requested: &NodeRecord,
    budget_tokens: u64,
) -> Result<ContextSlice> {
    let mut candidates: Vec<(NodeRecord, SliceReason)> =
        vec![(requested.clone(), SliceReason::Requested)];

    // The enclosing element, so the symbol is not read out of context.
    if let Some(parent_id) = &requested.parent_id
        && let Some(parent) = store.node(parent_id)?
    {
        candidates.push((parent, SliceReason::Enclosing));
    }

    // The one hop.
    for (node, kind, _weight, outgoing) in store.neighbors(&requested.id)? {
        let reason = match (kind, outgoing) {
            (EdgeKind::Calls, true) => SliceReason::Callee,
            (EdgeKind::Calls, false) => SliceReason::Caller,
            // `contains` outward from the requested node is its children, which
            // are already inside its own source, so they are not added again.
            (EdgeKind::Contains, true) => continue,
            (EdgeKind::Contains, false) => SliceReason::Enclosing,
            (EdgeKind::Imports, _) => SliceReason::Sibling,
        };
        candidates.push((node, reason));
    }

    // Siblings, which is what gives a symbol its immediate file context.
    if let Some(parent_id) = &requested.parent_id {
        for sibling in store.nodes_in_file(&requested.path)? {
            if sibling.parent_id.as_deref() == Some(parent_id.as_str())
                && sibling.id != requested.id
            {
                candidates.push((sibling, SliceReason::Sibling));
            }
        }
    }

    // De-duplicate, keeping the highest-priority reason for each node. A callee
    // that is also a sibling is a callee.
    candidates.sort_by_key(|(_, reason)| *reason);
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|(node, _)| seen.insert(node.id.clone()));

    let mut budget = TokenBudget::new(budget_tokens);
    let mut members = Vec::new();
    let mut omitted = Vec::new();
    let mut unreadable = Vec::new();
    for (node, reason) in candidates {
        // The requested node failing is the slice failing: an empty slice that
        // exits 0 would read as "nothing to know". A neighbor whose file has
        // moved on is listed as unreadable with the cause (not as a budget
        // omission), so one stale neighbor does not deny a worker its context.
        let source = match store.source_of(&node) {
            Ok(source) => source,
            Err(error) if reason == SliceReason::Requested => return Err(error),
            Err(error) => {
                unreadable.push((node, reason, error.to_string()));
                continue;
            }
        };
        let cost = estimate_tokens(&source);
        if budget.try_spend(&source) {
            members.push(SliceMember {
                node,
                reason,
                source,
                estimated_tokens: cost,
            });
        } else {
            omitted.push((node, reason));
        }
    }

    Ok(ContextSlice {
        requested: requested.qualified_name.clone(),
        members,
        omitted,
        unreadable,
        budget,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Indexer;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |rel: &str, body: &str| {
            let full = dir.path().join(rel);
            fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            fs::write(full, body).expect("write");
        };
        write(
            "src/lib.rs",
            "\
pub fn target() {
    helper();
}

pub fn helper() {
    // a helper with a body long enough to cost something
    let _ = 1 + 1;
}

pub fn caller() {
    target();
}

pub fn unrelated() {
    let _ = 2;
}
",
        );
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        Indexer::new(&store).index().expect("index");
        (dir, store)
    }

    #[test]
    fn a_slice_carries_the_symbol_and_its_one_hop_neighborhood() {
        let (_dir, store) = fixture();
        let slice = slice_for_symbol(&store, "target", 10_000).expect("slice");
        let by_reason = |reason: SliceReason| {
            slice
                .members
                .iter()
                .filter(|m| m.reason == reason)
                .map(|m| m.node.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(by_reason(SliceReason::Requested), vec!["target".to_owned()]);
        assert!(
            by_reason(SliceReason::Callee).contains(&"helper".to_owned()),
            "callees: {:?}",
            by_reason(SliceReason::Callee)
        );
        assert!(
            by_reason(SliceReason::Caller).contains(&"caller".to_owned()),
            "callers: {:?}",
            by_reason(SliceReason::Caller)
        );
    }

    #[test]
    fn a_slice_never_exceeds_its_budget() {
        let (_dir, store) = fixture();
        for budget in [1, 5, 20, 60, 200, 5_000] {
            let slice = slice_for_symbol(&store, "target", budget).expect("slice");
            assert!(
                slice.estimated_tokens() <= budget,
                "budget {budget} overspent to {}",
                slice.estimated_tokens()
            );
        }
    }

    #[test]
    fn a_tight_budget_keeps_the_requested_symbol_and_drops_neighbors() {
        // Priority order, not greedy by size. This is what stops a small budget
        // from being spent on whichever neighbor happened to be shortest.
        let (_dir, store) = fixture();
        let generous = slice_for_symbol(&store, "target", 10_000).expect("slice");
        let target_cost = generous
            .members
            .iter()
            .find(|m| m.reason == SliceReason::Requested)
            .expect("requested member")
            .estimated_tokens;

        let tight = slice_for_symbol(&store, "target", target_cost).expect("slice");
        assert_eq!(tight.members.len(), 1);
        assert_eq!(tight.members[0].reason, SliceReason::Requested);
        assert!(
            !tight.omitted.is_empty(),
            "omissions must be reported, not silent"
        );
    }

    #[test]
    fn omissions_are_reported_rather_than_dropped() {
        let (_dir, store) = fixture();
        let slice = slice_for_symbol(&store, "target", 30).expect("slice");
        let rendered = slice.render();
        assert!(
            rendered.contains("omitted for budget"),
            "a truncated slice must say so: {rendered}"
        );
    }

    #[test]
    fn a_zero_budget_yields_nothing_and_omits_everything() {
        // The degenerate case must not panic and must not smuggle one member
        // through by rounding.
        let (_dir, store) = fixture();
        let slice = slice_for_symbol(&store, "target", 0).expect("slice");
        assert!(slice.members.is_empty());
        assert!(!slice.omitted.is_empty());
        assert_eq!(slice.estimated_tokens(), 0);
    }

    #[test]
    fn an_unknown_symbol_raises() {
        let (_dir, store) = fixture();
        assert!(slice_for_symbol(&store, "no_such_symbol", 1000).is_err());
    }

    #[test]
    fn the_slice_reads_current_source_from_disk_after_a_reindex() {
        let (dir, store) = fixture();
        let before = slice_for_symbol(&store, "unrelated", 10_000).expect("slice");
        assert!(before.render().contains("let _ = 2;"));

        // Rewrite the file, keeping `unrelated` at the same lines. Until the
        // index catches up the slice refuses; afterward it serves the new bytes.
        let path = dir.path().join("src/lib.rs");
        let text = fs::read_to_string(&path).expect("read");
        fs::write(&path, text.replace("let _ = 2;", "let _ = 99;")).expect("write");
        assert!(slice_for_symbol(&store, "unrelated", 10_000).is_err());
        Indexer::new(&store).index().expect("reindex");
        let after = slice_for_symbol(&store, "unrelated", 10_000).expect("slice");
        assert!(
            after.render().contains("let _ = 99;"),
            "slice served stale source: {}",
            after.render()
        );
    }

    #[test]
    fn a_requested_node_in_an_edited_file_is_an_error_not_an_empty_slice() {
        let (dir, store) = fixture();
        // A line is inserted above every span, so the old spans now name
        // other text although they still fit inside the file.
        let path = dir.path().join("src/lib.rs");
        let text = fs::read_to_string(&path).expect("read");
        fs::write(&path, format!("// added\n{text}")).expect("write");
        let error = slice_for_symbol(&store, "unrelated", 10_000).expect_err("must refuse");
        assert!(matches!(error, GraphError::StaleFile { .. }), "{error:?}");
        assert!(error.to_string().contains("kgraf index"), "{error}");
    }

    #[test]
    fn a_stale_neighbor_is_unreadable_and_not_a_budget_omission() {
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |rel: &str, body: &str| {
            fs::create_dir_all(dir.path().join("src")).expect("mkdir");
            fs::write(dir.path().join(rel), body).expect("write");
        };
        write("src/a.rs", "pub fn entry() {\n    helper();\n}\n");
        write("src/b.rs", "pub fn helper() {\n    let _ = 1;\n}\n");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        Indexer::new(&store).index().expect("index");
        write(
            "src/b.rs",
            "// moved\npub fn helper() {\n    let _ = 1;\n}\n",
        );

        let slice = slice_for_symbol(&store, "entry", 10_000).expect("slice");
        assert!(slice.omitted.is_empty(), "{:?}", slice.omitted);
        assert_eq!(slice.unreadable.len(), 1);
        assert_eq!(slice.unreadable[0].0.name, "helper");
        let rendered = slice.render();
        assert!(rendered.contains("unreadable"), "{rendered}");
        assert!(!rendered.contains("omitted for budget"), "{rendered}");
    }

    #[test]
    fn an_ambiguous_name_is_refused_with_every_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        fs::write(dir.path().join("src/a.rs"), "pub fn twin() {}\n").expect("write");
        fs::write(dir.path().join("src/b.rs"), "pub fn twin() {}\n").expect("write");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        Indexer::new(&store).index().expect("index");
        let error = slice_for_symbol(&store, "twin", 1_000).expect_err("ambiguous");
        match &error {
            GraphError::AmbiguousSymbol { candidates, .. } => {
                assert_eq!(candidates.len(), 2, "{candidates:?}");
                assert!(candidates.iter().any(|c| c.contains("src/a.rs")));
                assert!(candidates.iter().any(|c| c.contains("src/b.rs")));
            }
            other => panic!("expected AmbiguousSymbol, got {other:?}"),
        }
    }

    #[test]
    fn an_exact_qualified_name_resolves_among_same_named_nodes() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        fs::write(
            dir.path().join("src/a.rs"),
            "pub struct S;\nimpl S {\n    pub fn go(&self) {}\n}\npub fn go() {}\n",
        )
        .expect("write");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        Indexer::new(&store).index().expect("index");
        let found = store.resolve_symbol("go").expect("resolve");
        assert!(found.len() >= 2, "{found:?}");
        // The bare name matches the free function's qualified name exactly.
        let node = resolve_unambiguous(&store, "go").expect("exact wins");
        assert_eq!(node.qualified_name, "go");
    }

    #[test]
    fn a_slice_is_smaller_than_the_whole_file_it_came_from() {
        // The 23 percent figure is not reproduced here; what is asserted is the
        // mechanism's direction, which the test-the-mechanism rule requires be tested rather than
        // commented.
        let (dir, store) = fixture();
        let whole = fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        let whole_cost = estimate_tokens(&whole);
        let slice = slice_for_symbol(&store, "target", whole_cost).expect("slice");
        assert!(
            slice.estimated_tokens() < whole_cost,
            "slice cost {} against whole-file {whole_cost}",
            slice.estimated_tokens()
        );
    }
}
