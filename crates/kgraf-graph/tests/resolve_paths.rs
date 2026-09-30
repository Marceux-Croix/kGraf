//! Symbol lookup takes the spellings a planner writes: Rust `::` paths, a
//! crate or module prefix, `crate::`/`self::`/`super::`, and the graph's own
//! dotted names. The graph stores methods as `Type.method` and does not prefix
//! the crate, so a path is matched by its tail, and any module segments it
//! names must be segments of the defining file's path.

use kgraf_graph::{GraphStore, Indexer};

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, GraphStore) {
    let dir = tempfile::tempdir().expect("dir");
    for (path, text) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, text).expect("write");
    }
    let store = GraphStore::open_in_memory(dir.path()).expect("store");
    Indexer::new(&store).index().expect("index");
    (dir, store)
}

const IFACE: &str = "pub struct Order { pub state: String }\n\
impl Order {\n    pub fn total_qty(&self) -> u32 { 0 }\n}\n\
pub trait Discount {\n    fn discount_cents(&self, order: &Order) -> i64;\n}\n";

const TAX: &str = "use iface::Order;\n\
pub fn state_tax_rate(_state: &str) -> f64 { 0.0 }\n\
pub struct TaxUs;\n\
impl TaxUs {\n    pub fn tax_cents(&self, _order: &Order) -> i64 { 0 }\n}\n";

/// A second crate defining a same-named symbol: a module prefix must choose.
const OTHER: &str = "pub struct Discount;\n";

fn scaffold() -> (tempfile::TempDir, GraphStore) {
    index(&[
        ("iface/src/lib.rs", IFACE),
        ("tax_us/src/lib.rs", TAX),
        ("other/src/lib.rs", OTHER),
    ])
}

fn paths(store: &GraphStore, name: &str) -> Vec<(String, String)> {
    store
        .resolve_symbol(name)
        .unwrap_or_else(|e| panic!("`{name}` did not resolve: {e}"))
        .into_iter()
        .map(|n| (n.path, n.qualified_name))
        .collect()
}

#[test]
fn a_rust_method_path_resolves_to_the_dotted_method() {
    let (_d, store) = scaffold();
    assert_eq!(
        paths(&store, "TaxUs::tax_cents"),
        vec![("tax_us/src/lib.rs".to_owned(), "TaxUs.tax_cents".to_owned())]
    );
    assert_eq!(
        paths(&store, "Order::total_qty"),
        vec![("iface/src/lib.rs".to_owned(), "Order.total_qty".to_owned())]
    );
}

#[test]
fn a_crate_prefix_resolves_and_chooses_among_same_named_symbols() {
    let (_d, store) = scaffold();
    // Both crates define `Discount`; the bare name returns both.
    assert_eq!(paths(&store, "Discount").len(), 2);
    assert_eq!(
        paths(&store, "iface::Discount"),
        vec![("iface/src/lib.rs".to_owned(), "Discount".to_owned())]
    );
    assert_eq!(
        paths(&store, "tax_us::state_tax_rate"),
        vec![("tax_us/src/lib.rs".to_owned(), "state_tax_rate".to_owned())]
    );
    assert_eq!(
        paths(&store, "tax_us::TaxUs::tax_cents"),
        vec![("tax_us/src/lib.rs".to_owned(), "TaxUs.tax_cents".to_owned())]
    );
    // The struct and its `impl` block are both nodes named `TaxUs`, as for the
    // bare name.
    assert_eq!(paths(&store, "crate::TaxUs"), paths(&store, "TaxUs"));
    assert!(
        paths(&store, "crate::TaxUs")
            .iter()
            .all(|(p, q)| p == "tax_us/src/lib.rs" && q == "TaxUs")
    );
}

#[test]
fn a_prefix_that_names_no_path_of_the_symbol_does_not_resolve() {
    let (_d, store) = scaffold();
    // `TaxUs` exists, but not under a module or crate called `iface`: picking
    // it anyway would hand a worker the wrong code.
    assert!(store.resolve_symbol("iface::TaxUs").is_err());
    assert!(store.resolve_symbol("Missing::tax_cents").is_err());
    assert!(store.resolve_symbol("TaxUs::missing").is_err());
}

#[test]
fn the_exact_names_still_resolve_first() {
    let (_d, store) = scaffold();
    assert_eq!(
        paths(&store, "TaxUs.tax_cents"),
        vec![("tax_us/src/lib.rs".to_owned(), "TaxUs.tax_cents".to_owned())]
    );
    assert_eq!(paths(&store, "state_tax_rate").len(), 1);
}

/// `crate::`, `self::` and `super::` name the requesting file's own crate: the
/// lookup keeps only that crate's symbol, and with no requester it refuses to
/// pick among crates, listing the candidates.
#[test]
fn a_crate_relative_path_resolves_in_the_requesters_crate_only() {
    let (_d, store) = index(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"iface\", \"other\"]\n",
        ),
        ("iface/Cargo.toml", "[package]\nname = \"iface\"\n"),
        ("iface/src/lib.rs", IFACE),
        ("iface/src/extra.rs", "pub fn helper() {}\n"),
        ("other/Cargo.toml", "[package]\nname = \"other\"\n"),
        ("other/src/lib.rs", OTHER),
    ]);
    let from = |name: &str, file: Option<&str>| {
        store
            .resolve_symbol_from(name, file)
            .map(|v| v.into_iter().map(|n| n.path).collect::<Vec<_>>())
    };
    assert_eq!(
        from("crate::Discount", Some("iface/src/extra.rs")).expect("resolves"),
        vec!["iface/src/lib.rs".to_owned()]
    );
    assert_eq!(
        from("crate::Discount", Some("other/src/lib.rs")).expect("resolves"),
        vec!["other/src/lib.rs".to_owned()]
    );
    assert_eq!(
        from("self::Discount", Some("other/src/lib.rs")).expect("resolves"),
        vec!["other/src/lib.rs".to_owned()]
    );
    // The requester's crate has no such symbol: nothing, never another crate's.
    assert!(matches!(
        from("crate::helper", Some("other/src/lib.rs")),
        Err(kgraf_graph::GraphError::UnknownSymbol { .. })
    ));
    match from("crate::Discount", None) {
        Err(kgraf_graph::GraphError::AmbiguousSymbol { candidates, .. }) => {
            assert_eq!(candidates.len(), 2, "{candidates:?}");
        }
        other => panic!("expected the candidates marked ambiguous, got {other:?}"),
    }
    // One crate holds it: no requester is needed.
    assert_eq!(
        from("crate::helper", None).expect("one crate"),
        vec!["iface/src/extra.rs".to_owned()]
    );
    // A bare name and a named crate are not crate-relative.
    assert_eq!(from("Discount", None).expect("bare").len(), 2);
    assert_eq!(
        from("iface::Discount", Some("other/src/lib.rs")).expect("named crate"),
        vec!["iface/src/lib.rs".to_owned()]
    );
}
