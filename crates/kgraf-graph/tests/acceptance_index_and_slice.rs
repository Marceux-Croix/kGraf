//! Indexing and slicing acceptance, clause by clause.
//!
//! The acceptance criterion reads:
//!
//! > Index a real multi-language repository of at least 50k lines. Resolve a
//! > symbol by name to its node. Return that node's source plus its one-hop
//! > neighborhood as a context slice sized to a token budget. Re-index after an
//! > edit touches only the changed subgraph, **and detects a content change that
//! > preserves both modification time and size**. Query the graph by structural
//! > pattern, not just text. Layout positions are stable across a reindex.
//!
//! Every clause has a test here. The scale clause needs a repository this test
//! cannot carry, so it runs against a real checkout named by the
//! `KGRAF_ACCEPTANCE_REPO` environment variable and **fails loudly when the
//! variable points somewhere unusable**, rather than passing vacuously. With the
//! variable unset it reports that the clause was not exercised. That is the
//! honest shape for a criterion whose input is not in this repository; the
//! scale figures print on stdout.

use kgraf_graph::{GraphStore, Indexer, Pattern, slice};
use std::fs;
use std::path::Path;

/// A small but genuinely multi-language tree, for the clauses that do not need
/// scale.
fn fixture() -> (tempfile::TempDir, GraphStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let write = |rel: &str, body: &str| {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, body).expect("write");
    };
    write(
        "crates/core/src/store.rs",
        "\
pub struct Store {
    open: bool,
}

impl Store {
    pub fn open() -> Self {
        Store { open: true }
    }

    pub fn close(&mut self) {
        self.open = false;
    }
}

pub fn bootstrap() -> Store {
    Store::open()
}
",
    );
    write(
        "scripts/migrate.py",
        "\
class Migration:
    def apply(self):
        validate()

def validate():
    return True
",
    );
    write(
        "ui/src/panel.ts",
        "\
export interface PanelProps {
    title: string;
}

export class Panel {
    render(): void {
        layout();
    }
}

export function layout(): void {}
",
    );
    write(
        "docs/design.md",
        "# Design\n\nbody\n\n## Store\n\nabout the store\n",
    );
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    Indexer::new(&store).index().expect("index");
    (dir, store)
}

#[test]
fn clause_resolve_a_symbol_by_name_to_its_node() {
    let (_dir, store) = fixture();
    let found = store.resolve_symbol("bootstrap").expect("resolve");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, "crates/core/src/store.rs");
    assert!(found[0].start_line > 1);

    // A qualified name resolves too, and outranks a bare match.
    let qualified = store.resolve_symbol("Store.close").expect("resolve");
    assert_eq!(qualified[0].qualified_name, "Store.close");

    // Symbols from every indexed language are reachable by name.
    for symbol in ["Migration", "validate", "Panel", "layout", "Design"] {
        store
            .resolve_symbol(symbol)
            .unwrap_or_else(|e| panic!("{symbol}: {e}"));
    }

    // An absent symbol raises rather than returning an empty list that a caller
    // might read as "no neighbors".
    assert!(store.resolve_symbol("not_a_symbol").is_err());
}

#[test]
fn clause_a_context_slice_is_the_node_plus_one_hop_sized_to_a_budget() {
    let (_dir, store) = fixture();
    let generous = slice::slice_for_symbol(&store, "bootstrap", 100_000).expect("slice");
    let rendered = generous.render();
    assert!(
        rendered.contains("Store::open()"),
        "the node's own source: {rendered}"
    );
    assert!(
        generous
            .members
            .iter()
            .any(|m| m.node.qualified_name == "Store.open"),
        "the one hop to its callee: {:#?}",
        generous
            .members
            .iter()
            .map(|m| &m.node.qualified_name)
            .collect::<Vec<_>>()
    );

    // Sized to a budget, at every budget, with the overflow reported.
    for budget in [0, 1, 10, 50, 250, 2_000, 100_000] {
        let slice = slice::slice_for_symbol(&store, "bootstrap", budget).expect("slice");
        assert!(
            slice.estimated_tokens() <= budget,
            "budget {budget} overspent to {}",
            slice.estimated_tokens()
        );
    }
    let tight = slice::slice_for_symbol(&store, "bootstrap", 12).expect("slice");
    assert!(
        tight.render().contains("omitted for budget") || tight.members.is_empty(),
        "a truncated slice must say what it dropped"
    );
}

#[test]
fn clause_reindex_touches_only_the_changed_subgraph() {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..20 {
        let full = dir.path().join(format!("src/m{i}.rs"));
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(&full, format!("pub fn f{i}() {{}}\n")).expect("write");
    }
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    let indexer = Indexer::new(&store);
    let first = indexer.index().expect("first");
    assert_eq!(first.files_added, 20);

    fs::write(
        dir.path().join("src/m7.rs"),
        "pub fn f7() {}\npub fn added() {}\n",
    )
    .expect("write");
    let second = indexer.index().expect("second");
    assert_eq!(second.files_changed, 1);
    assert_eq!(second.files_unchanged, 19);
    assert!(
        second.change_log_rows * 5 < first.change_log_rows,
        "reindex wrote {} rows against a first pass of {}",
        second.change_log_rows,
        first.change_log_rows
    );

    // And a pass over a tree that did not change writes nothing at all.
    let third = indexer.index().expect("third");
    assert_eq!(third.change_log_rows, 0);
}

#[test]
fn clause_a_content_change_preserving_mtime_and_size_is_detected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let full = dir.path().join("lib.rs");
    fs::write(&full, "pub fn before() {}\n").expect("write");
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    let indexer = Indexer::new(&store);
    indexer.index().expect("first");

    let stamp = fs::metadata(&full)
        .expect("metadata")
        .modified()
        .expect("mtime");
    let size = fs::metadata(&full).expect("metadata").len();

    // Same length, different content, timestamp restored: the exact case the
    // reference implementation misses.
    fs::write(&full, "pub fn during() {}\n").expect("rewrite");
    let handle = fs::File::options().write(true).open(&full).expect("open");
    handle.set_modified(stamp).expect("restore mtime");
    drop(handle);

    let after = fs::metadata(&full).expect("metadata");
    assert_eq!(after.len(), size, "the test requires an identical size");
    assert_eq!(
        after.modified().expect("mtime"),
        stamp,
        "the test requires an identical mtime"
    );

    let report = indexer.index().expect("second");
    assert_eq!(report.files_changed, 1);
    store
        .resolve_symbol("during")
        .expect("the new symbol is indexed");
    assert!(
        store.resolve_symbol("before").is_err(),
        "the old symbol survived the reindex"
    );
}

#[test]
fn clause_the_graph_is_queryable_by_structural_pattern_not_just_text() {
    let (_dir, store) = fixture();

    // Text alone cannot express this: `Panel.render` and `Store.close` are both
    // methods with similar names, and only one calls `layout`.
    let callers = Pattern::parse("kind=method -calls-> name=layout")
        .expect("parse")
        .run(&store, 50)
        .expect("run");
    let names: Vec<_> = callers
        .iter()
        .map(|m| m.node.qualified_name.clone())
        .collect();
    assert_eq!(names, vec!["Panel.render".to_owned()], "got {names:?}");

    // Shape in the other direction.
    let members = Pattern::parse("kind=method <-contains- kind=impl name=Store")
        .expect("parse")
        .run(&store, 50)
        .expect("run");
    assert_eq!(members.len(), 2, "{members:#?}");

    // Scoped by location, across languages.
    let ui = Pattern::parse("path~^ui/ kind=interface")
        .expect("parse")
        .run(&store, 50)
        .expect("run");
    assert_eq!(ui.len(), 1);
    assert_eq!(ui[0].node.name, "PanelProps");

    // A malformed query raises rather than degrading to a substring search.
    assert!(Pattern::parse("kind=function -nosuchedge-> name=x").is_err());
}

#[test]
fn clause_layout_positions_are_stable_across_a_reindex() {
    let dir = tempfile::tempdir().expect("tempdir");
    let write = |rel: &str, body: &str| {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, body).expect("write");
    };
    write("a/one.rs", "pub fn stays() {}\n");
    write("b/two.rs", "pub fn elsewhere() {}\n");
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    let indexer = Indexer::new(&store);
    indexer.index().expect("first");

    let position_of = |name: &str| {
        let node = store.resolve_symbol(name).expect("resolve")[0].clone();
        store.position(&node.id).expect("position").expect("cached")
    };
    let stays_before = position_of("stays");
    let elsewhere_before = position_of("elsewhere");

    // Insert a node in one region and reindex.
    write("a/one.rs", "pub fn stays() {}\npub fn inserted() {}\n");
    indexer.index().expect("second");

    assert_eq!(position_of("stays"), stays_before);
    assert_eq!(
        position_of("elsewhere"),
        elsewhere_before,
        "a node in another region moved when a node was inserted"
    );
}

/// The scale clause. Real repository, real languages, real line count.
///
/// Reports its figures on stdout so an acceptance run can be transcribed into
/// a written record, and fails when pointed at something unusable rather than
/// quietly passing.
#[test]
fn clause_index_a_real_multi_language_repository_of_at_least_50k_lines() {
    let Some(repo) = kgraf_core::legacy::env_var("KGRAF_ACCEPTANCE_REPO") else {
        eprintln!(
            "NOT EXERCISED: set KGRAF_ACCEPTANCE_REPO to a real checkout to run the \
             scale clause. The clause runs only against a real checkout."
        );
        return;
    };
    let root = Path::new(&repo);
    assert!(
        root.is_dir(),
        "KGRAF_ACCEPTANCE_REPO={} is not a directory",
        root.display()
    );

    let store = GraphStore::open_in_memory(root).expect("store");
    let started = std::time::Instant::now();
    let report = Indexer::new(&store).index().expect("index");
    let elapsed = started.elapsed();

    // Language breakdown, because "multi-language" is the part of the clause a
    // single total would hide.
    let mut by_language: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
    {
        let conn_paths = store.file_paths().expect("paths");
        for path in conn_paths {
            let file = store.file(&path).expect("file").expect("present");
            let key = file
                .language
                .map(|l| l.as_str().to_owned())
                .unwrap_or_else(|| "unknown".to_owned());
            let entry = by_language.entry(key).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += file.line_count as u64;
        }
    }

    println!("--- scale clause ---");
    println!("repository:  {}", root.display());
    println!("files:       {}", report.files_seen);
    println!("lines:       {}", report.lines);
    println!("nodes:       {}", report.nodes);
    println!("edges:       {}", report.edges);
    println!("wall clock:  {:.2}s", elapsed.as_secs_f64());
    println!("calls unresolved:     {}", report.calls_unresolved);
    println!("calls too ambiguous:  {}", report.calls_too_ambiguous);
    for (language, (files, lines)) in &by_language {
        println!("  {language:<12} {files:>6} files {lines:>9} lines");
    }

    assert!(
        report.lines >= 50_000,
        "the clause requires at least 50k indexed lines; got {}",
        report.lines
    );
    let languages = by_language.len();
    assert!(
        languages >= 2,
        "the clause requires a multi-language repository; got {languages}"
    );
    assert!(
        report.nodes > report.files_seen,
        "chunking produced nothing"
    );

    // A second pass over the same unchanged checkout must be cheap and silent,
    // which is the property that makes the index usable on a real repository at
    // all.
    let second = Indexer::new(&store).index().expect("reindex");
    assert_eq!(second.files_changed, 0);
    assert_eq!(second.files_added, 0);
    assert_eq!(
        second.change_log_rows, 0,
        "a reindex of an unchanged checkout wrote {} change-log rows",
        second.change_log_rows
    );
    println!("reindex of the unchanged checkout: 0 change-log rows");
}
