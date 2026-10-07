//! Reconciling the plan against the code: where the project actually is.
//!
//! Doneness is **derived from code and tests, never read from a status column**
//! (decision 0019). Each work item names the evidence that would
//! exist when it is done (paths, symbols, tests, plus the path, symbol and test
//! checks of its criteria). An item is:
//!
//! - **Complete** when every named path and symbol exists, at least one test is
//!   named, and every named test exists and passed;
//! - **NotStarted** when none of its code exists and none of its tests exists;
//! - **Partial** otherwise, including code with no passing tests.
//!
//! **Only code counts as code.** A symbol is a graph node that is not a file, a
//! Markdown section or an `impl` block, in a file that is not Markdown: a heading
//! named `export_csv` in a roadmap is not the function. A test is such a node that
//! is marked as a test (a `#[test]`-family attribute on the lines above it, or
//! pytest's `test_` name prefix; a helper that merely lives under `tests/` is not),
//! matched by name or qualified name.
//!
//! Test outcomes come from a [`TestResults`] source. kGraf does not run a
//! project's tests to interpret it; without recorded results every test is
//! `NotRun`, so nothing derives Complete. That is the conservative direction: a
//! resumed item is re-verified, never skipped on the documents' word.
//!
//! A difference between what the documents claim and what is derived is a
//! [`Discrepancy`].

use crate::model::{Answer, Check, Claim, WorkItem};
use kgraf_graph::{GraphError, GraphStore, NodeKind, NodeRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A test's outcome as a results source knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Passed,
    Failed,
    /// No recorded run.
    NotRun,
}

/// Where test outcomes come from.
pub trait TestResults: Send + Sync {
    fn outcome(&self, test: &str) -> TestOutcome;
}

/// No recorded runs: every test is `NotRun`.
pub struct NoTestRuns;

impl TestResults for NoTestRuns {
    fn outcome(&self, _test: &str) -> TestOutcome {
        TestOutcome::NotRun
    }
}

/// Outcomes recorded elsewhere (a gate run, a verifier), by test name.
#[derive(Clone, Debug, Default)]
pub struct RecordedTests(pub BTreeMap<String, bool>);

impl TestResults for RecordedTests {
    fn outcome(&self, test: &str) -> TestOutcome {
        match self.0.get(test) {
            Some(true) => TestOutcome::Passed,
            Some(false) => TestOutcome::Failed,
            None => TestOutcome::NotRun,
        }
    }
}

/// Derived doneness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Doneness {
    NotStarted,
    Partial,
    Complete,
}

/// One named test and what is known of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestState {
    pub name: String,
    /// Whether the graph has a definition by that name.
    pub exists: bool,
    pub outcome: TestOutcome,
}

/// One item's derived state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    pub item: String,
    pub doneness: Doneness,
    /// Paths and symbols found.
    pub present: Vec<String>,
    /// Paths and symbols not found.
    pub missing: Vec<String>,
    pub tests: Vec<TestState>,
    /// Set when the item names no evidence at all, so its doneness could not be
    /// derived and defaults to `NotStarted`.
    pub underivable: bool,
}

/// A difference between the documents' claim and the derived state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discrepancy {
    pub item: String,
    pub claimed: Claim,
    pub derived: Doneness,
    pub detail: String,
}

/// The item's evidence, its criteria's checks included, deduplicated.
fn evidence_of(item: &WorkItem) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut paths = item.evidence.paths.clone();
    let mut symbols = item.evidence.symbols.clone();
    let mut tests = item.evidence.tests.clone();
    for c in &item.acceptance {
        match &c.check {
            Some(Check::Path { path }) => paths.push(path.clone()),
            Some(Check::Symbol { name }) => symbols.push(name.clone()),
            Some(Check::Test { name }) => tests.push(name.clone()),
            _ => {}
        }
    }
    for v in [&mut paths, &mut symbols, &mut tests] {
        v.retain(|s| !s.trim().is_empty());
        v.sort();
        v.dedup();
    }
    (paths, symbols, tests)
}

/// Candidates for `name` that are code: not a file, section or `impl` node, and
/// not in a Markdown file.
fn code_nodes(store: &GraphStore, name: &str) -> Result<Vec<NodeRecord>, GraphError> {
    let found = match store.resolve_symbol(name) {
        Ok(found) => found,
        Err(GraphError::UnknownSymbol { .. }) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(found
        .into_iter()
        .filter(|n| {
            let not_code = matches!(n.kind, NodeKind::File | NodeKind::Section | NodeKind::Impl)
                || n.path.ends_with(".md")
                || n.path.ends_with(".markdown");
            !not_code
        })
        .collect())
}

pub(crate) fn symbol_exists(store: &GraphStore, name: &str) -> Result<bool, GraphError> {
    Ok(!code_nodes(store, name)?.is_empty())
}

/// Attributes that mark a Rust function as a test.
const TEST_ATTRIBUTES: &[&str] = &[
    "#[test]",
    "#[tokio::test",
    "#[rstest",
    "#[test_case",
    "#[async_std::test",
    "#[wasm_bindgen_test",
    "@pytest.mark",
];

/// Whether a code node is a test: it carries a test marker, a test attribute on
/// its first line or among the attribute, decorator and comment lines directly
/// above it (`#[test]`, `#[tokio::test]`, `@pytest.mark`, ...), or pytest's
/// `test_` name prefix. Its location is not a marker: a helper under
/// `tests/` is not a test.
fn is_test_node(store: &GraphStore, node: &NodeRecord) -> Result<bool, GraphError> {
    if !matches!(
        node.kind,
        NodeKind::Function | NodeKind::Method | NodeKind::Decorated
    ) {
        return Ok(false);
    }
    // pytest's marker is the name.
    if node.name.starts_with("test_") {
        return Ok(true);
    }
    // A file indexed from another tree (a git worktree) is read from that tree, the
    // same one the graph's own source reads use.
    // A database error is raised: reading it as "no recorded root" would look for
    // the file in the store's own tree and answer "not a test" without saying why.
    let root = store
        .file_source_root(&node.path)?
        .unwrap_or_else(|| store.root().to_path_buf());
    let Ok(text) = std::fs::read_to_string(root.join(&node.path)) else {
        return Ok(false);
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = (node.start_line as usize).saturating_sub(1);
    // A stale index can name a line the file no longer has: then the node is not a
    // test here.
    if start >= lines.len() {
        return Ok(false);
    }
    let is_marker = |l: &str| {
        TEST_ATTRIBUTES
            .iter()
            .any(|a| l.trim_start().starts_with(a))
    };
    // The node's own first line (a span may start at its attribute), then the
    // contiguous attribute, decorator and comment lines directly above it; a blank
    // line or code ends them, so another function's `#[test]` never counts.
    if is_marker(lines[start]) {
        return Ok(true);
    }
    for l in lines[..start].iter().rev() {
        let t = l.trim_start();
        let attached = t.starts_with("#[") || t.starts_with('@') || t.starts_with("//");
        if !attached {
            return Ok(false);
        }
        if is_marker(l) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn test_exists(store: &GraphStore, name: &str) -> Result<bool, GraphError> {
    for node in code_nodes(store, name)? {
        if is_test_node(store, &node)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn path_exists(store: &GraphStore, path: &str) -> Result<bool, GraphError> {
    let path = path.trim_start_matches("./");
    if store.file(path)?.is_some() {
        return Ok(true);
    }
    // A file the graph does not index (a config file, a directory).
    Ok(crate::model::relative_inside(path) && resolves_inside(store, path))
}

/// Whether `rel` exists and resolves inside the project root:
/// only plain relative components, and the path with every symbolic link
/// followed still under the root. A link to a directory outside the project
/// does not make a cited path exist here.
pub(crate) fn resolves_inside(store: &GraphStore, rel: &str) -> bool {
    use std::path::Component;
    let rel = rel.trim_start_matches("./");
    let plain = !rel.is_empty()
        && std::path::Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !plain {
        return false;
    }
    let (Ok(full), Ok(root)) = (
        kgraf_core::plain_path::canonicalize(store.root().join(rel)),
        kgraf_core::plain_path::canonicalize(store.root()),
    ) else {
        return false;
    };
    full.starts_with(root)
}

/// Derive one item's state.
pub fn derive(
    store: &GraphStore,
    item: &WorkItem,
    tests: &dyn TestResults,
) -> Result<ItemState, GraphError> {
    let (paths, symbols, test_names) = evidence_of(item);
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for p in &paths {
        if path_exists(store, p)? {
            present.push(p.clone());
        } else {
            missing.push(p.clone());
        }
    }
    for s in &symbols {
        if symbol_exists(store, s)? {
            present.push(s.clone());
        } else {
            missing.push(s.clone());
        }
    }
    let mut test_states = Vec::new();
    for t in &test_names {
        test_states.push(TestState {
            name: t.clone(),
            exists: test_exists(store, t)?,
            outcome: tests.outcome(t),
        });
    }
    let underivable = paths.is_empty() && symbols.is_empty() && test_names.is_empty();
    let any_code = !present.is_empty() || test_states.iter().any(|t| t.exists);
    let all_code = missing.is_empty();
    let tests_pass = !test_states.is_empty()
        && test_states
            .iter()
            .all(|t| t.exists && t.outcome == TestOutcome::Passed);
    let doneness = if underivable || !any_code {
        Doneness::NotStarted
    } else if all_code && tests_pass {
        Doneness::Complete
    } else {
        Doneness::Partial
    };
    Ok(ItemState {
        item: item.id.clone(),
        doneness,
        present,
        missing,
        tests: test_states,
        underivable,
    })
}

fn discrepancy(item: &WorkItem, state: &ItemState) -> Option<Discrepancy> {
    let detail = match (item.documents_claim, state.doneness) {
        (Claim::Complete, Doneness::NotStarted) => {
            "the documents call it complete, but none of its code exists".to_owned()
        }
        (Claim::Complete, Doneness::Partial) if state.missing.is_empty() => {
            let failing: Vec<String> = state
                .tests
                .iter()
                .map(|t| {
                    format!(
                        "{} ({})",
                        t.name,
                        if !t.exists {
                            "absent"
                        } else {
                            match t.outcome {
                                TestOutcome::Passed => "passed",
                                TestOutcome::Failed => "failed",
                                TestOutcome::NotRun => "not run",
                            }
                        }
                    )
                })
                .collect();
            if failing.is_empty() {
                "the documents call it complete and its code exists, but it names no test, so it \
                 derives as partial"
                    .to_owned()
            } else {
                format!(
                    "the documents call it complete and its code exists, but its tests have not \
                     passed: {}",
                    failing.join(", ")
                )
            }
        }
        (Claim::Complete, Doneness::Partial) => format!(
            "the documents call it complete, but this is missing: {}",
            state.missing.join(", ")
        ),
        (Claim::NotStarted | Claim::Unstated, Doneness::Complete) => {
            "its code and passing tests exist, but the documents do not record it as done"
                .to_owned()
        }
        (Claim::NotStarted, Doneness::Partial) => {
            format!(
                "the documents call it not started, but this exists: {}",
                state.present.join(", ")
            )
        }
        _ => return None,
    };
    Some(Discrepancy {
        item: item.id.clone(),
        claimed: item.documents_claim,
        derived: state.doneness,
        detail,
    })
}

/// Derive every item and list the discrepancies.
pub fn reconcile(
    store: &GraphStore,
    answer: &Answer,
    tests: &dyn TestResults,
) -> Result<(Vec<ItemState>, Vec<Discrepancy>), GraphError> {
    let mut states = Vec::new();
    let mut discrepancies = Vec::new();
    for item in &answer.work_items {
        let state = derive(store, item, tests)?;
        if let Some(d) = discrepancy(item, &state) {
            discrepancies.push(d);
        }
        states.push(state);
    }
    Ok((states, discrepancies))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, p: &str, t: &str) {
        let p = root.join(p);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
        std::fs::write(p, t).expect("write");
    }

    fn item(tests: &[&str]) -> WorkItem {
        serde_json::from_value(serde_json::json!({"id": "a", "title": "a",
            "instruction": "do", "acceptance": [{"text": "x"}], "evidence": {"tests": tests}}))
        .expect("item")
    }

    /// A file shortened after
    /// indexing made the line window start past its end, and slicing panicked.
    #[test]
    fn a_stale_index_does_not_panic_and_its_node_is_not_a_test() {
        let dir = tempfile::tempdir().expect("dir");
        let db = tempfile::tempdir().expect("db");
        let mut src = "// pad\n".repeat(50);
        src.push_str("#[test]\nfn exports_csv() {}\n");
        write(dir.path(), "src/lib.rs", &src);
        let store = GraphStore::open(&db.path().join("g.db"), dir.path()).expect("store");
        kgraf_graph::Indexer::new(&store).index().expect("index");
        write(dir.path(), "src/lib.rs", "fn x() {}\n");
        let s = derive(&store, &item(&["exports_csv"]), &NoTestRuns).expect("derive");
        assert!(!s.tests[0].exists);
    }

    /// A database error while looking up a file's source root is raised. It is
    /// not read as "no recorded root", which would answer "not a test".
    #[test]
    fn a_source_root_lookup_error_is_raised_not_read_as_no_test() {
        let dir = tempfile::tempdir().expect("dir");
        let db = tempfile::tempdir().expect("db");
        write(dir.path(), "src/lib.rs", "#[test]\nfn exports_csv() {}\n");
        let store = GraphStore::open(&db.path().join("g.db"), dir.path()).expect("store");
        kgraf_graph::Indexer::new(&store).index().expect("index");
        assert!(test_exists(&store, "exports_csv").expect("found"));
        rusqlite::Connection::open(db.path().join("g.db"))
            .expect("second connection")
            .execute_batch(
                "DROP TABLE file_source_roots; CREATE TABLE file_source_roots (unrelated TEXT);",
            )
            .expect("damage");
        assert!(
            test_exists(&store, "exports_csv").is_err(),
            "a damaged lookup answered instead of failing"
        );
    }

    /// A test indexed from another tree (a git worktree) is read from that tree,
    /// not from the store's root, where the file may not exist.
    #[test]
    fn a_test_indexed_from_another_tree_is_found_there() {
        let checkout = tempfile::tempdir().expect("checkout");
        let other = tempfile::tempdir().expect("other");
        let db = tempfile::tempdir().expect("db");
        write(other.path(), "src/lib.rs", "#[test]\nfn exports_csv() {}\n");
        let store = GraphStore::open(&db.path().join("g.db"), checkout.path()).expect("store");
        kgraf_graph::Indexer::new(&store)
            .with_source_root(other.path())
            .index_paths(&["src/lib.rs".to_owned()])
            .expect("index");
        let s = derive(&store, &item(&["exports_csv"]), &NoTestRuns).expect("derive");
        assert!(
            s.tests[0].exists,
            "the marker in the other tree was not read"
        );
    }

    /// A
    /// helper under `tests/` counted as a test. A test needs a marker.
    #[test]
    fn a_helper_under_tests_is_not_a_test_and_a_marked_one_is() {
        let dir = tempfile::tempdir().expect("dir");
        let db = tempfile::tempdir().expect("db");
        write(dir.path(), "src/lib.rs", "pub fn exports_csv() {}\n");
        write(
            dir.path(),
            "tests/support/mod.rs",
            "pub fn make_fixture() {}\n\n#[tokio::test]\nasync fn round_trips() {}\n",
        );
        write(
            dir.path(),
            "tests/test_io.py",
            "def helper():\n    pass\n\ndef test_reads():\n    pass\n",
        );
        let store = GraphStore::open(&db.path().join("g.db"), dir.path()).expect("store");
        kgraf_graph::Indexer::new(&store).index().expect("index");
        let s = derive(
            &store,
            &item(&[
                "exports_csv",
                "make_fixture",
                "round_trips",
                "helper",
                "test_reads",
            ]),
            &NoTestRuns,
        )
        .expect("derive");
        let exists = |n: &str| s.tests.iter().find(|t| t.name == n).expect(n).exists;
        assert!(!exists("exports_csv"));
        assert!(
            !exists("make_fixture"),
            "a helper under tests/ is not a test"
        );
        assert!(!exists("helper"));
        assert!(exists("round_trips"));
        assert!(exists("test_reads"));
    }
}
