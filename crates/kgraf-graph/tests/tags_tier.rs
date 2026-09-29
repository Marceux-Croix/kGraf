//! The tags tier: Go, Java, C, C++,
//! Ruby, C# and PHP are chunked from each grammar crate's own `TAGS_QUERY`. Each
//! test indexes a small tree and checks the mechanism end to end: the definitions
//! the query tags become nodes of the expected kind, nested as the source nests
//! them, and a call inside one becomes a `calls` edge to the other.

use kgraf_graph::{EdgeKind, GraphStore, Indexer, Language, NodeKind, Tier};

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

/// The one node named `qualified` (a dotted path), with its kind.
fn kind_of(store: &GraphStore, name: &str, qualified: &str) -> NodeKind {
    let found = store.resolve_symbol(name).expect("resolve");
    found
        .iter()
        .find(|n| n.qualified_name == qualified)
        .unwrap_or_else(|| panic!("no `{qualified}` in {found:#?}"))
        .kind
}

fn calls(store: &GraphStore, caller: &str) -> Vec<String> {
    let node = store.resolve_symbol(caller).expect("resolve")[0].clone();
    store
        .neighbors(&node.id)
        .expect("neighbors")
        .into_iter()
        .filter(|(_, kind, _, outgoing)| *kind == EdgeKind::Calls && *outgoing)
        .map(|(n, _, _, _)| n.qualified_name)
        .collect()
}

#[test]
fn every_tags_language_is_the_tags_tier_and_its_query_compiles() {
    let tags: Vec<Language> = Language::all()
        .iter()
        .copied()
        .filter(|l| l.tier() == Tier::Tags)
        .collect();
    assert_eq!(
        tags,
        [
            Language::Go,
            Language::Java,
            Language::C,
            Language::Cpp,
            Language::Ruby,
            Language::CSharp,
            Language::Php
        ]
    );
    for l in tags {
        // Chunking an empty file compiles the query; a mismatch raises.
        kgraf_graph::chunk::chunk_source("x", l, "")
            .unwrap_or_else(|e| panic!("{}: {e}", l.as_str()));
    }
}

#[test]
fn go() {
    let (_d, s) = index(&[(
        "cmd/main.go",
        "package main\n\ntype Store struct{}\n\n// Open opens.\nfunc (s *Store) Open() { helper() }\n\nfunc helper() {}\n",
    )]);
    assert_eq!(kind_of(&s, "Store", "Store"), NodeKind::TypeAlias);
    assert_eq!(kind_of(&s, "Open", "Open"), NodeKind::Method);
    assert_eq!(kind_of(&s, "helper", "helper"), NodeKind::Function);
    assert_eq!(calls(&s, "Open"), ["helper"]);
}

#[test]
fn java() {
    let (_d, s) = index(&[(
        "src/Store.java",
        "class Store {\n  void open() { helper(); }\n  static void helper() {}\n}\ninterface Api { void run(); }\n",
    )]);
    assert_eq!(kind_of(&s, "Store", "Store"), NodeKind::Class);
    assert_eq!(kind_of(&s, "open", "Store.open"), NodeKind::Method);
    assert_eq!(kind_of(&s, "Api", "Api"), NodeKind::Interface);
    assert_eq!(kind_of(&s, "run", "Api.run"), NodeKind::Method);
    assert_eq!(calls(&s, "open"), ["Store.helper"]);
}

#[test]
fn c_widens_a_declarator_to_its_function_so_the_body_call_lands_on_it() {
    let (_d, s) = index(&[(
        "src/lib.c",
        "struct point { int x; };\nstatic int helper(int a) { return a; }\nint run(void) {\n  return helper(1);\n}\n",
    )]);
    assert_eq!(kind_of(&s, "point", "point"), NodeKind::Class);
    assert_eq!(kind_of(&s, "helper", "helper"), NodeKind::Function);
    let run = &s.resolve_symbol("run").expect("resolve")[0];
    assert_eq!(
        (run.start_line, run.end_line),
        (3, 5),
        "the whole definition, not the declarator line"
    );
    assert_eq!(calls(&s, "run"), ["helper"]);
}

#[test]
fn cpp_nests_a_member_function_as_a_method_and_calls_c_in_its_family() {
    let (_d, s) = index(&[
        (
            "src/widget.cpp",
            "class Widget {\n public:\n  void grow() { c_helper(); }\n};\n",
        ),
        ("src/helper.c", "void c_helper(void) {}\n"),
    ]);
    assert_eq!(kind_of(&s, "Widget", "Widget"), NodeKind::Class);
    assert_eq!(kind_of(&s, "grow", "Widget.grow"), NodeKind::Method);
    assert_eq!(
        calls(&s, "grow"),
        ["c_helper"],
        "C and C++ are one language family"
    );
}

#[test]
fn ruby() {
    let (_d, s) = index(&[(
        "lib/tools.rb",
        "module Tools\n  class Store\n    def open\n      helper(1)\n    end\n\n    def helper(x)\n    end\n  end\nend\n",
    )]);
    assert_eq!(kind_of(&s, "Tools", "Tools"), NodeKind::Module);
    assert_eq!(kind_of(&s, "Store", "Tools.Store"), NodeKind::Class);
    assert_eq!(kind_of(&s, "open", "Tools.Store.open"), NodeKind::Method);
    assert_eq!(calls(&s, "open"), ["Tools.Store.helper"]);
}

#[test]
fn csharp() {
    let (_d, s) = index(&[(
        "src/Store.cs",
        "namespace App {\n  class Store {\n    void Open() { Helper(); }\n    static void Helper() {}\n  }\n}\n",
    )]);
    assert_eq!(kind_of(&s, "App", "App"), NodeKind::Module);
    assert_eq!(kind_of(&s, "Store", "App.Store"), NodeKind::Class);
    assert_eq!(kind_of(&s, "Open", "App.Store.Open"), NodeKind::Method);
    assert_eq!(calls(&s, "Open"), ["App.Store.Helper"]);
}

#[test]
fn php() {
    let (_d, s) = index(&[(
        "src/store.php",
        "<?php\nclass Store {\n  function open() { helper(); \\Lib\\util(); }\n}\nfunction helper() {}\nfunction util() {}\n",
    )]);
    assert_eq!(kind_of(&s, "Store", "Store"), NodeKind::Class);
    assert_eq!(kind_of(&s, "open", "Store.open"), NodeKind::Method);
    assert_eq!(kind_of(&s, "helper", "helper"), NodeKind::Function);
    assert_eq!(
        calls(&s, "open"),
        ["helper", "util"],
        "a namespaced call too"
    );
}

/// Identity (decision 0049) holds in the tags tier: an edit to a body keeps every
/// id, and adding a function above another does not rebind it.
#[test]
fn tags_tier_ids_survive_a_body_edit_and_an_insertion() {
    let before = kgraf_graph::chunk::chunk_source(
        "a.go",
        Language::Go,
        "package a\n\nfunc alpha() {}\n\nfunc beta() { alpha() }\n",
    )
    .expect("chunk");
    let after = kgraf_graph::chunk::chunk_source(
        "a.go",
        Language::Go,
        "package a\n\nfunc first() {}\n\nfunc alpha() { x := 1; _ = x }\n\nfunc beta() { alpha() }\n",
    )
    .expect("chunk");
    for name in ["alpha", "beta"] {
        let id = |cs: &[kgraf_graph::chunk::Chunk]| {
            cs.iter()
                .find(|c| c.name == name)
                .map(|c| (c.id.clone(), c.identity_stable))
                .expect(name)
        };
        assert_eq!(id(&before), id(&after), "{name}");
        assert!(id(&after).1);
    }
}

/// A slice states a tags-tier member's fidelity; a full-tier one is unchanged.
#[test]
fn a_slice_names_the_tags_tier() {
    let (_d, s) = index(&[
        (
            "a/main.go",
            "package a\n\nfunc run() { work() }\n\nfunc work() {}\n",
        ),
        ("b/lib.rs", "fn solo() {}\n"),
    ]);
    let go = kgraf_graph::slice::slice_for_symbol(&s, "run", 2000)
        .expect("slice")
        .render();
    assert!(
        go.contains("(run) [tags tier: calls resolved by name only]"),
        "{go}"
    );
    let rust = kgraf_graph::slice::slice_for_symbol(&s, "solo", 2000)
        .expect("slice")
        .render();
    assert!(!rust.contains("tags tier"), "{rust}");
}

/// The call gate holds across the new families: a Go call does not reach a Java
/// method of the same name.
#[test]
fn a_go_call_does_not_resolve_to_a_java_method() {
    let (_d, s) = index(&[
        ("a/main.go", "package a\n\nfunc run() { work() }\n"),
        ("b/Work.java", "class W { static void work() {} }\n"),
    ]);
    assert!(calls(&s, "run").is_empty());
}
