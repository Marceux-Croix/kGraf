//! Decision 0078 amendment (2026-10-06, second): how a statement names a
//! symbol. A recorded handshake held on `symbol_unprovided` for a symbol its
//! provider named in a list ("`` `vectorops` `` (a Python package) and
//! `` `dotproduct` ``"), written in double-backtick code spans. The record is
//! replayed verbatim; each relaxation has a REAL companion proving a true
//! finding still holds. The Python, JavaScript and Ruby spellings of a name
//! (import lines, call forms, untyped `def`, `Class#method`) are read here
//! too.

use super::*;

#[derive(serde::Deserialize)]
struct Replay {
    interface: InterfaceContract,
    handshake: Vec<HandshakeStatement>,
    recorded_findings: Vec<HandshakeFinding>,
    writes: BTreeMap<String, Vec<WriteTarget>>,
    depends_on: BTreeMap<String, Vec<String>>,
}

fn holding(f: &[HandshakeFinding]) -> Vec<HandshakeFinding> {
    f.iter().filter(|x| x.holds()).cloned().collect()
}

fn unprovided(f: &[HandshakeFinding]) -> Vec<String> {
    f.iter()
        .filter_map(|x| match x {
            HandshakeFinding::SymbolUnprovided { symbol } => Some(symbol.clone()),
            _ => None,
        })
        .collect()
}

fn statement(party: &str, provide: &[&str], expect: &[&str]) -> HandshakeStatement {
    let own = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect();
    HandshakeStatement {
        party: party.into(),
        provide: own(provide),
        expect: own(expect),
        ..Default::default()
    }
}

/// An interface over `symbols`, parties `p` (producer) and `c` (consumer),
/// with the given stub files.
fn ic(symbols: &[&str], stub: Vec<FileOverlay>) -> InterfaceContract {
    InterfaceContract {
        kind: crate::checks::InterfaceKind::Behavior,
        id: "i_forms".into(),
        version: 1,
        parties: vec!["p".into(), "c".into()],
        symbols: symbols.iter().map(|s| (*s).to_owned()).collect(),
        stub,
        wrong: vec![],
        reference: vec![],
        consumer: vec![],
        tests: vec![],
        compile: vec![],
        test: vec![],
        consulted: vec![],
        replaces: vec![],
    }
}

fn writes(of: &[(&str, &[&str])]) -> BTreeMap<String, Vec<WriteTarget>> {
    of.iter()
        .map(|(party, paths)| {
            (
                (*party).to_owned(),
                paths
                    .iter()
                    .map(|p| WriteTarget {
                        path: (*p).into(),
                        node: None,
                    })
                    .collect(),
            )
        })
        .collect()
}

// ------------------------------------------------------------ the record

/// A recorded Python packaging run: the re-plan's interface
/// `re1.i_c1_to_c2` over `vectorops` and `dotproduct`, `re1.c1` writing the
/// stub file. `re1.c1` said "I will provide `` `vectorops` `` (a Python
/// package) and `` `dotproduct` ``. The symbol `` `dotproduct` `` refers to a
/// function with signature `` dotproduct(...) -> float ``." The reader took
/// `vectorops` as the only subject, read `dotproduct` as a claim about it,
/// and lost the signature in its double-backtick span, so the run held on
/// `symbol_unprovided` `dotproduct`. Now both symbols are provided and
/// nothing holds; the consumer's assumption about `pypiserver` is still a
/// warning. The fixture is the trace's interface, statements and findings,
/// verbatim, with the plan's declared writes and dependency.
#[test]
fn the_recorded_two_symbol_provision_provides_both_and_nothing_holds() {
    let r: Replay = serde_json::from_str(include_str!(
        "../tests/fixtures/handshake/two-symbols-in-one-provision.json"
    ))
    .expect("fixture");
    assert_eq!(
        holding(&r.recorded_findings),
        vec![HandshakeFinding::SymbolUnprovided {
            symbol: "dotproduct".into()
        }],
        "the record held on `dotproduct` alone"
    );
    let now = compare_handshake_ordered(
        &r.interface,
        &r.handshake,
        &r.writes,
        &ordered_pairs(&r.depends_on),
    );
    assert_eq!(holding(&now), vec![], "{now:#?}");
    assert!(
        now.iter().any(|f| matches!(f,
            HandshakeFinding::OutsideSurface { symbol, .. } if symbol == "pypiserver")),
        "{now:#?}"
    );
}

/// The recorded provision, read directly: two subjects, and the signature in
/// its double-backtick span is a claim about `dotproduct`.
#[test]
fn the_recorded_provision_reads_two_subjects_and_the_signature() {
    let r: Replay = serde_json::from_str(include_str!(
        "../tests/fixtures/handshake/two-symbols-in-one-provision.json"
    ))
    .expect("fixture");
    let surface: BTreeSet<&str> = r.interface.symbols.iter().map(String::as_str).collect();
    let text = &r.handshake[0].provide[0];
    let reading = read_sentence(text, &surface, &BTreeSet::new());
    assert_eq!(reading.subject, Some("vectorops"));
    let (sym, claims) = reading.also.first().expect("a second subject");
    assert_eq!(*sym, "dotproduct", "{reading:?}");
    assert!(
        claims.contains("fn(list[float], list[float]) -> float"),
        "{claims:?}"
    );
}

// ------------------------------------------------------------ code spans

#[test]
fn double_backtick_spans_are_anchors() {
    assert_eq!(
        anchors("I will provide `` `vectorops` `` and ``dotproduct(a: int) -> int``."),
        vec![
            "vectorops".to_owned(),
            "dotproduct(a: int) -> int".to_owned()
        ]
    );
    // A span may hold a single backtick when a longer run delimits it.
    assert_eq!(anchors("`` a`b ``"), vec!["a`b".to_owned()]);
    // A run with no closer is prose, as CommonMark reads it.
    assert_eq!(anchors("an unclosed `x and `y`"), vec!["x and".to_owned()]);
    assert_eq!(anchors("no ``closer"), Vec::<String>::new());
    assert!(prose_words("I will not provide ``x``").contains(&"not".to_owned()));
    assert!(!prose_words("I will not provide ``x``").contains(&"x".to_owned()));
}

// ------------------------------------------------------------ listed subjects

/// "I will provide `a` and `b`" provides both; a list joiner may carry a
/// parenthetical or a kind word, and a later clause may lead with the
/// symbol.
#[test]
fn every_listed_or_leading_symbol_of_a_provision_is_provided() {
    let surface = ["alpha", "beta", "gamma"];
    for provide in [
        "I will provide `alpha`, `beta` and `gamma`.",
        "I will provide `alpha` (a package), `beta` as well as the function `gamma`.",
        "I will provide `alpha` and `beta`. The symbol `gamma` returns `int`.",
        "I will provide `alpha` and `beta`; I will also provide `gamma`.",
    ] {
        let f = compare_handshake(
            &ic(&surface, vec![]),
            &[statement("p", &[provide], &[]), statement("c", &[], &[])],
            &BTreeMap::new(),
        );
        assert_eq!(unprovided(&f), Vec::<String>::new(), "{provide}: {f:#?}");
    }
}

/// REAL: a symbol named as a claim about another ("which returns a
/// `Config`"), governed by a use word ("and tests for `beta`"), or listed
/// with a symbol the party tests rather than provides, is not provided.
#[test]
fn a_symbol_named_as_a_claim_or_a_use_is_still_unprovided() {
    for (provide, missing) in [
        (
            "I will provide `alpha`, which returns a `Config`.",
            "Config",
        ),
        ("I will provide `alpha` that returns `Config`.", "Config"),
        ("I will provide `alpha` and tests for `beta`.", "beta"),
        ("I will provide tests for `beta` and `alpha`.", "alpha"),
        ("I will provide `alpha`. It returns a `Config`.", "Config"),
        ("I will provide `alpha`, which calls `beta`.", "beta"),
    ] {
        let surface = ["alpha", "beta", "Config"];
        let f = compare_handshake(
            &ic(&surface, vec![]),
            &[statement("p", &[provide], &[]), statement("c", &[], &[])],
            &BTreeMap::new(),
        );
        assert!(
            unprovided(&f).contains(&missing.to_owned()),
            "{provide}: {f:#?}"
        );
    }
}

/// REAL: a listed symbol whose stub file another party writes is not
/// provided by the lister, and a listed symbol two parties both provide is
/// still double provided.
#[test]
fn a_listed_symbol_keeps_the_writer_rule_and_the_double_provider_rule() {
    let two_files = ic(
        &["alpha", "beta"],
        vec![
            FileOverlay::new("a.py", "def alpha():\n    return 0\n"),
            FileOverlay::new("b.py", "def beta():\n    return 0\n"),
        ],
    );
    let w = writes(&[("p", &["a.py"]), ("c", &["b.py"])]);
    let f = compare_handshake(
        &two_files,
        &[
            statement("p", &["I will provide `alpha` and `beta`."], &[]),
            statement("c", &[], &[]),
        ],
        &w,
    );
    assert_eq!(unprovided(&f), vec!["beta".to_owned()], "{f:#?}");
    let f = compare_handshake(
        &two_files,
        &[
            statement("p", &["I will provide `alpha` and `beta`."], &[]),
            statement("c", &["I will provide `beta`."], &[]),
        ],
        &w,
    );
    assert_eq!(
        holding(&f),
        vec![],
        "the writer of b.py alone provides it: {f:#?}"
    );
    let f = compare_handshake(
        &ic(&["alpha", "beta"], vec![]),
        &[
            statement("p", &["I will provide `alpha` and `beta`."], &[]),
            statement("c", &["I will provide `beta`."], &[]),
        ],
        &BTreeMap::new(),
    );
    assert!(
        f.contains(&HandshakeFinding::DoubleProvided {
            symbol: "beta".into(),
            parties: vec!["c".into(), "p".into()],
        }),
        "{f:#?}"
    );
}

/// REAL: a provider that never names the symbol is still held on it.
#[test]
fn a_symbol_the_provider_never_names_is_still_unprovided() {
    let f = compare_handshake(
        &ic(&["vectorops", "dotproduct"], vec![]),
        &[
            statement(
                "p",
                &["I will provide `vectorops` (a Python package)."],
                &[],
            ),
            statement(
                "c",
                &[],
                &["I assume `vectorops` contains the `dotproduct` function."],
            ),
        ],
        &BTreeMap::new(),
    );
    assert_eq!(unprovided(&f), vec!["dotproduct".to_owned()], "{f:#?}");
}

// ------------------------------------------------------------ language forms

#[test]
fn import_lines_name_their_symbols() {
    let surface: BTreeSet<&str> = ["dotproduct", "Config"].into();
    let named = |a: &str| imported(a).iter().find_map(|n| resolve(n, &surface));
    for a in [
        "from vectorops import dotproduct",
        "from vectorops import (norm, dotproduct as dp)",
        "from .vectorops import dotproduct",
        "import { dotproduct } from \"vectorops\"",
        "import dotproduct from './vectorops'",
        "use vectorops::dotproduct;",
        "use vectorops::{norm, dotproduct};",
        "pub use crate::ops::dotproduct as dp;",
    ] {
        assert_eq!(named(a), Some("dotproduct"), "{a}");
    }
    assert_eq!(named("import vectorops"), None);
    assert_eq!(named("from config import Config"), Some("Config"));
    assert_eq!(named("import \"fmt\""), None);
    assert_eq!(named("dotproduct is imported"), None);
}

#[test]
fn qualified_names_in_each_language_resolve() {
    let surface: BTreeSet<&str> = ["dotproduct"].into();
    for n in [
        "vectorops.dotproduct",
        "vectorops::dotproduct",
        "Vectorops#dotproduct",
        "pkg.vectorops.dotproduct",
    ] {
        assert_eq!(resolve(n, &surface), Some("dotproduct"), "{n}");
    }
    // Names are compared as written: Python, Rust and most languages are
    // case-sensitive, so `DotProduct` is another name.
    assert_eq!(resolve("vectorops.DotProduct", &surface), None);
    assert_eq!(resolve("dotproduct_v2", &surface), None);
}

#[test]
fn call_forms_and_untyped_defs_name_the_symbol_and_claim_no_arity() {
    let read = |a: &str| signature(a).map(|s| (s.name, s.canonical, s.types));
    assert_eq!(
        read("dotproduct(a, b)"),
        Some(("dotproduct".into(), None, vec![]))
    );
    assert_eq!(
        read("vectorops.dotproduct([1, 2], [3, 4])"),
        Some(("vectorops.dotproduct".into(), None, vec![]))
    );
    assert_eq!(
        read("def dotproduct(a, b) -> float:"),
        Some(("dotproduct".into(), None, vec!["float".into()]))
    );
    assert_eq!(read("dotproduct()").map(|s| s.1), Some(Some("fn()".into())));
    // Defaults, keyword markers and `cls` are not types.
    assert_eq!(
        read(
            "def scale(cls, v: list[float], k: float = 1.0, *, strict: bool = False) -> list[float]"
        )
        .map(|s| s.1),
        Some(None),
        "the bare `*` marker is untyped"
    );
    assert_eq!(
        read("def scale(cls, v: list[float], k: float = 1.0) -> list[float]").map(|s| s.1),
        Some(Some("fn(list[float], float) -> list[float]".into()))
    );
    // JavaScript and TypeScript: the `function` keyword and a `: T` return.
    assert_eq!(
        read("function scale(v: number[], k: number = 1): number[]"),
        Some((
            "scale".into(),
            Some("fn(number[], number) -> number[]".into()),
            vec!["number[]".into(), "number".into(), "number[]".into()]
        ))
    );
    assert_eq!(
        read("export function dotproduct(a, b)").map(|s| (s.0, s.1)),
        Some(("dotproduct".into(), None))
    );
    // Types written without names still read as types.
    assert_eq!(
        read("double(i64) -> i64").map(|s| s.1),
        Some(Some("fn(i64) -> i64".into()))
    );
    assert_eq!(
        read("fn(x) -> i64").map(|s| s.1),
        Some(Some("fn(x) -> i64".into())),
        "an unnamed function type lists types only"
    );
}

/// An expecter that names the symbol by its call form or its import line
/// agrees with a provider that states a typed signature; one that states a
/// different return type still disagrees.
#[test]
fn a_call_form_or_import_expectation_agrees_and_a_changed_type_disagrees() {
    let surface = ["dotproduct"];
    let provides =
        "I will provide `def dotproduct(a: list[float], b: list[float]) -> float` in the package.";
    for expects in [
        "I assume `vectorops.dotproduct(a, b)` returns the dot product.",
        "I assume `from vectorops import dotproduct` works.",
        "I assume `dotproduct([1, 2], [3, 4])` returns a `float`.",
    ] {
        let f = compare_handshake(
            &ic(&surface, vec![]),
            &[
                statement("p", &[provides], &[]),
                statement("c", &[], &[expects]),
            ],
            &BTreeMap::new(),
        );
        assert_eq!(f, vec![], "{expects}: {f:#?}");
    }
    let f = compare_handshake(
        &ic(&surface, vec![]),
        &[
            statement("p", &[provides], &[]),
            statement("c", &[], &["I assume `dotproduct(a, b)` returns an `int`."]),
        ],
        &BTreeMap::new(),
    );
    assert!(
        matches!(&f[..], [HandshakeFinding::Disagree { symbol, .. }] if symbol == "dotproduct"),
        "{f:#?}"
    );
}

/// A provider that names its symbol only by an import line provides it.
#[test]
fn a_provision_written_as_an_import_line_provides_the_symbol() {
    let f = compare_handshake(
        &ic(&["dotproduct"], vec![]),
        &[
            statement(
                "p",
                &["I will provide `from vectorops import dotproduct` for import."],
                &[],
            ),
            statement("c", &[], &[]),
        ],
        &BTreeMap::new(),
    );
    assert_eq!(unprovided(&f), Vec::<String>::new(), "{f:#?}");
}

/// A default value is cut at its own `=`, not at one inside a type, and an
/// arrow in the type does not hide it.
#[test]
fn a_default_is_dropped_from_the_parameter_type() {
    assert_eq!(
        param("cb: fn(i64) -> i64 = double", true),
        Param::Type("fn(i64) -> i64".into())
    );
    assert_eq!(
        param("f: (x: number) => number = id", true),
        Param::Type("(x: number) => number".into())
    );
    assert_eq!(param("k: float = 1.0", true), Param::Type("float".into()));
    assert_eq!(param("k=1.0", true), Param::Untyped);
    assert_eq!(param("cls", true), Param::Receiver);
}

const KIND_PROVIDES: &str = "I will provide `dotproduct` function that takes two `list`s of numbers (`int` or `float`) and returns their dot product as a number.";

/// A consumer that echoes the schema's kind label for the symbol states the
/// symbol's kind, not a property the provider must repeat.
#[test]
fn a_kind_label_the_provider_matches_is_not_a_disagreement() {
    let f = compare_handshake(
        &ic(&["dotproduct"], vec![]),
        &[
            statement("p", &[KIND_PROVIDES], &[]),
            statement(
                "c",
                &[],
                &[
                    "I assume you provide `dotproduct` as a `callable_symbol` within the package namespace.",
                ],
            ),
        ],
        &BTreeMap::new(),
    );
    assert_eq!(holding(&f), vec![], "{f:#?}");
}

/// A kind label does not excuse a wrong claim beside it, and a kind the
/// provider contradicts is still a disagreement.
#[test]
fn a_kind_label_beside_a_wrong_claim_still_disagrees() {
    for expects in [
        "I assume you provide `dotproduct` as a `callable_symbol` that returns a `str`.",
        "I assume you provide `dotproduct` as a `type_symbol`.",
    ] {
        let f = compare_handshake(
            &ic(&["dotproduct"], vec![]),
            &[
                statement("p", &[KIND_PROVIDES], &[]),
                statement("c", &[], &[expects]),
            ],
            &BTreeMap::new(),
        );
        assert!(
            matches!(&f[..], [HandshakeFinding::Disagree { symbol, .. }] if symbol == "dotproduct"),
            "{expects}: {f:#?}"
        );
    }
}
