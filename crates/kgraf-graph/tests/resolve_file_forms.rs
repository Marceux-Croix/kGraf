//! Names written against a file (`path::Name`, `path::Type::method`), a crate
//! package name, and a bare file stem. Each resolves to the one node it names;
//! several matches come back as candidates, never as a pick; a name that
//! matches no part of the file resolves to nothing.

use kgraf_graph::{GraphError, GraphStore, Indexer};

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
const TAX: &str = "pub fn state_tax_rate(_s: &str) -> f64 { 0.0 }\n\
pub struct TaxUs;\nimpl TaxUs {\n    pub fn tax_cents(&self) -> i64 { 0 }\n}\n";
const OTHER: &str = "pub struct Discount;\npub fn state_tax_rate() {}\n";

fn scaffold() -> (tempfile::TempDir, GraphStore) {
    index(&[
        ("iface/src/lib.rs", IFACE),
        ("tax_us/src/lib.rs", TAX),
        ("other/src/lib.rs", OTHER),
        ("other/src/util.rs", "pub fn helper() {}\n"),
    ])
}

fn found(store: &GraphStore, name: &str) -> Vec<(String, String)> {
    store
        .resolve_file_form(name)
        .unwrap_or_else(|e| panic!("`{name}` did not resolve: {e}"))
        .into_iter()
        .map(|n| (n.path, n.qualified_name))
        .collect()
}

fn one(path: &str, q: &str) -> Vec<(String, String)> {
    vec![(path.to_owned(), q.to_owned())]
}

#[test]
fn a_path_and_a_symbol_resolve_to_the_symbol_in_that_file() {
    let (_d, store) = scaffold();
    // The struct and its `impl` block are one symbol: the type is taken.
    assert_eq!(
        found(&store, "iface/src/lib.rs::Order"),
        one("iface/src/lib.rs", "Order")
    );
    assert_eq!(
        found(&store, "tax_us/src/lib.rs::state_tax_rate"),
        one("tax_us/src/lib.rs", "state_tax_rate")
    );
    // The same name in another file is not reached through this file's path.
    assert_eq!(
        found(&store, "other/src/lib.rs::state_tax_rate"),
        one("other/src/lib.rs", "state_tax_rate")
    );
}

#[test]
fn a_path_then_type_and_method_resolves_the_method() {
    let (_d, store) = scaffold();
    assert_eq!(
        found(&store, "tax_us/src/lib.rs::TaxUs::tax_cents"),
        one("tax_us/src/lib.rs", "TaxUs.tax_cents")
    );
}

/// A trait's method with no body is not a node of its own; the trait holds its
/// signature and is the node the name resolves to.
#[test]
fn a_bodyless_trait_method_resolves_to_its_trait() {
    let (_d, store) = scaffold();
    for name in [
        "Discount::discount_cents",
        "iface::Discount::discount_cents",
        "iface/src/lib.rs::Discount::discount_cents",
    ] {
        let hits = store
            .resolve_file_form(name)
            .unwrap_or_else(|e| panic!("`{name}`: {e}"));
        assert_eq!(hits.len(), 1, "{name}: {hits:?}");
        assert_eq!(hits[0].path, "iface/src/lib.rs");
        assert_eq!(hits[0].qualified_name, "Discount");
    }
    // The struct `Discount` in another crate declares no such method, and a
    // method the trait lacks is a miss.
    for name in [
        "Discount::missing",
        "other::Discount::discount_cents",
        "Order::discount_cents",
        "other/src/lib.rs::Discount::discount_cents",
    ] {
        assert!(
            matches!(
                store.resolve_file_form(name),
                Err(GraphError::UnknownSymbol { .. })
            ),
            "`{name}` should not resolve"
        );
    }
}

#[test]
fn a_path_shared_by_several_files_returns_every_candidate() {
    let (_d, store) = scaffold();
    // `src/lib.rs` is in three crates; only two define `state_tax_rate`.
    let both = found(&store, "src/lib.rs::state_tax_rate");
    assert_eq!(both.len(), 2, "{both:?}");
    assert!(both.iter().any(|(p, _)| p == "tax_us/src/lib.rs"));
    assert!(both.iter().any(|(p, _)| p == "other/src/lib.rs"));
}

#[test]
fn a_path_that_does_not_hold_the_symbol_resolves_to_nothing() {
    let (_d, store) = scaffold();
    // `TaxUs` exists, in another file: not handed back for this path.
    for name in [
        "iface/src/lib.rs::TaxUs",
        "iface/src/lib.rs::Missing",
        "missing/src/lib.rs::Order",
        "iface/src/lib.rs::Order::missing",
        "iface/src/lib.rs::",
        "::Order",
    ] {
        assert!(
            matches!(
                store.resolve_file_form(name),
                Err(GraphError::UnknownSymbol { .. })
            ),
            "`{name}` should not resolve"
        );
    }
}

#[test]
fn a_bare_stem_resolves_only_when_one_file_has_it() {
    let (_d, store) = scaffold();
    let util = store.resolve_file_form("util").expect("one file");
    assert_eq!(util.len(), 1);
    assert_eq!(util[0].path, "other/src/util.rs");
    // `lib` is the stem of three files: all returned, none chosen.
    let lib = store.resolve_file_form("lib").expect("candidates");
    assert_eq!(lib.len(), 3, "{lib:?}");
    assert!(matches!(
        store.resolve_file_form("nosuchstem"),
        Err(GraphError::UnknownSymbol { .. })
    ));
}

#[test]
fn names_that_resolved_before_resolve_as_before() {
    let (_d, store) = scaffold();
    let paths = |name: &str| -> Vec<String> {
        store
            .resolve_symbol(name)
            .expect("resolves")
            .into_iter()
            .map(|n| format!("{}:{}", n.path, n.qualified_name))
            .collect()
    };
    assert_eq!(
        paths("TaxUs.tax_cents"),
        vec!["tax_us/src/lib.rs:TaxUs.tax_cents"]
    );
    assert_eq!(
        paths("TaxUs::tax_cents"),
        vec!["tax_us/src/lib.rs:TaxUs.tax_cents"]
    );
    assert_eq!(paths("iface::Discount"), vec!["iface/src/lib.rs:Discount"]);
    assert_eq!(paths("Discount").len(), 2);
    // The file forms are not part of `resolve_symbol`.
    assert!(store.resolve_symbol("iface/src/lib.rs::Order").is_err());
    assert!(store.resolve_symbol("util").is_err());
}

/// A crate whose package name differs from its folder: the manifest names it.
#[test]
fn a_package_name_from_the_manifest_is_a_crate_prefix() {
    let (_d, store) = index(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        (
            "crates/counter/Cargo.toml",
            "[package]\nname = \"fx-counter\"\nversion = \"0.1.0\"\n",
        ),
        ("crates/counter/src/lib.rs", "pub trait Pricing {}\n"),
        (
            "crates/other/Cargo.toml",
            "[package]\nname = \"fx_other\"\nversion = \"0.1.0\"\n",
        ),
        ("crates/other/src/lib.rs", "pub trait Pricing {}\n"),
    ]);
    let hit = store
        .resolve_symbol("fx_counter::Pricing")
        .expect("package");
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].path, "crates/counter/src/lib.rs");
    let hit = store.resolve_symbol("fx-other::Pricing").expect("package");
    assert_eq!(hit[0].path, "crates/other/src/lib.rs");
    assert!(store.resolve_symbol("fx_nothing::Pricing").is_err());
}
