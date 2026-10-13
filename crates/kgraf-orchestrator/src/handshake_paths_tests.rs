//! Interfaces over files: a symbol written as a bare file name, and an
//! expectation that states only a file's kind. Two recorded handshakes over
//! a Python package held a plan whose parties agreed; each is replayed
//! verbatim, and each relaxation has a companion proving a true finding
//! still holds.

use super::*;

#[derive(serde::Deserialize)]
struct Replay {
    interface: InterfaceContract,
    handshake: Vec<HandshakeStatement>,
    recorded_findings: Vec<HandshakeFinding>,
    writes: BTreeMap<String, Vec<WriteTarget>>,
    depends_on: BTreeMap<String, Vec<String>>,
}

fn replay(text: &str) -> (Replay, Vec<HandshakeFinding>) {
    let r: Replay = serde_json::from_str(text).expect("fixture");
    let now = compare_handshake_ordered(
        &r.interface,
        &r.handshake,
        &r.writes,
        &ordered_pairs(&r.depends_on),
    );
    (r, now)
}

fn holding(f: &[HandshakeFinding]) -> Vec<HandshakeFinding> {
    f.iter().filter(|x| x.holds()).cloned().collect()
}

fn file(path: &str) -> FileOverlay {
    FileOverlay {
        path: path.into(),
        content: "x = 1\n".into(),
    }
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

fn ic(symbols: &[&str], stub: &[&str]) -> InterfaceContract {
    InterfaceContract {
        kind: crate::checks::InterfaceKind::Behavior,
        id: "i_files".into(),
        version: 1,
        parties: vec!["p".into(), "c".into()],
        symbols: symbols.iter().map(|s| (*s).to_owned()).collect(),
        stub: stub.iter().map(|p| file(p)).collect(),
        wrong: vec![],
        reference: stub.iter().map(|p| file(p)).collect(),
        consumer: vec![],
        tests: vec![],
        compile: vec![],
        test: vec![],
        consulted: vec![],
        replaces: vec![],
    }
}

fn writes_of(producer: &[&str]) -> BTreeMap<String, Vec<WriteTarget>> {
    let targets = producer
        .iter()
        .map(|p| WriteTarget {
            path: (*p).into(),
            node: None,
        })
        .collect();
    BTreeMap::from([("p".to_owned(), targets), ("c".to_owned(), vec![])])
}

// ------------------------------------------------------------ kind only

/// The re-plan's interface `re1.i_vecops_build` over `vectorops/__init__.py`
/// and `pyproject.toml`. The consumer said "I assume you provide
/// `vectorops/__init__.py` as type `Code` and `pyproject.toml` as type
/// `Config`"; the provider said what each file holds, in two sentences. The
/// comparison required `Code`, `pyproject.toml` and `Config` among the claims
/// of the provider's `vectorops/__init__.py` sentence, so the run held on
/// `disagree`. A file has no type in a language, so a type word after
/// "type" or "kind" names the file's kind, and the other file is provided by
/// the same party in its own sentence. Nothing holds now.
#[test]
fn the_recorded_kind_only_expectation_agrees_with_the_provider() {
    let (r, now) = replay(include_str!(
        "../tests/fixtures/handshake/kind-only-expectation.json"
    ));
    assert!(
        matches!(&holding(&r.recorded_findings)[..],
            [HandshakeFinding::Disagree { symbol, .. }] if symbol == "vectorops/__init__.py"),
        "the record held on one disagreement: {:#?}",
        r.recorded_findings
    );
    assert_eq!(holding(&now), vec![], "{now:#?}");
}

/// REAL companion: a provider that states another kind for the file still
/// disagrees.
#[test]
fn a_file_kind_the_provider_contradicts_still_disagrees() {
    let i = ic(&["app/__init__.py"], &["app/__init__.py"]);
    let s = [
        statement(
            "p",
            &["I will provide `app/__init__.py` as type `Config`."],
            &[],
        ),
        statement(
            "c",
            &[],
            &["I assume you provide `app/__init__.py` as type `Code`."],
        ),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["app/__init__.py"]));
    assert!(
        matches!(&holding(&f)[..],
            [HandshakeFinding::Disagree { symbol, .. }] if symbol == "app/__init__.py"),
        "{f:#?}"
    );
}

/// REAL companion: on a symbol that is not a file, "of type `f64`" is a type
/// the provider must state, as before.
#[test]
fn a_type_of_a_named_item_is_still_a_claim() {
    let i = ic(&["rate"], &["app/rate.py"]);
    let s = [
        statement("p", &["I will provide `rate`, a constant."], &[]),
        statement("c", &[], &["I assume you provide `rate` of type `f64`."]),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["app/rate.py"]));
    assert!(
        matches!(&holding(&f)[..],
            [HandshakeFinding::Disagree { symbol, .. }] if symbol == "rate"),
        "{f:#?}"
    );
}

/// REAL companion: a second file the expectation lists is satisfied only by
/// a party that provides it; one no party provides is still a finding.
#[test]
fn a_listed_file_nobody_provides_is_still_a_finding() {
    let i = ic(&["app/__init__.py", "pyproject.toml"], &["app/__init__.py"]);
    let s = [
        statement(
            "p",
            &["I will provide `app/__init__.py` with the code."],
            &[],
        ),
        statement(
            "c",
            &[],
            &["I assume you provide `app/__init__.py` as type `Code` and `pyproject.toml`."],
        ),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["app/__init__.py"]));
    assert!(
        f.contains(&HandshakeFinding::SymbolUnprovided {
            symbol: "pyproject.toml".into()
        }),
        "{f:#?}"
    );
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::Disagree { symbol, .. } if symbol == "app/__init__.py")),
        "{f:#?}"
    );
}

// ------------------------------------------------------------ bare names

/// The first plan's interface `i_pkg` listed `__init__.py` and `setup.py`
/// while its stub and reference files were `vectorops/__init__.py` and
/// `setup.py`. The provider named `vectorops/__init__.py`, which matched no
/// surface symbol, so the run held on `symbol_unprovided` `__init__.py` and
/// on the consumer's `unprovided`. The bare name now resolves to the one stub
/// path with that file name; the provider's build outputs off the surface
/// stay a warning.
#[test]
fn the_recorded_bare_file_name_resolves_to_the_stub_path() {
    let (r, now) = replay(include_str!(
        "../tests/fixtures/handshake/bare-file-name-symbol.json"
    ));
    assert!(
        r.recorded_findings
            .contains(&HandshakeFinding::SymbolUnprovided {
                symbol: "__init__.py".into()
            }),
        "{:#?}",
        r.recorded_findings
    );
    assert_eq!(holding(&now), vec![], "{now:#?}");
    assert!(
        now.iter().any(|f| matches!(f,
            HandshakeFinding::Unanchored { party, .. } if party == "c_build_package")),
        "{now:#?}"
    );
}

#[test]
fn file_symbols_resolve_a_unique_bare_name_and_name_an_ambiguous_one() {
    let one = ic(
        &["__init__.py", "setup.py"],
        &["vectorops/__init__.py", "setup.py"],
    );
    let r = resolve_file_symbols(&one);
    assert_eq!(r.symbols, vec!["vectorops/__init__.py", "setup.py"]);
    assert_eq!(
        r.resolved,
        vec![("__init__.py".to_owned(), "vectorops/__init__.py".to_owned())]
    );
    assert!(r.ambiguous.is_empty());

    let two = ic(&["__init__.py"], &["a/__init__.py", "b/__init__.py"]);
    let r = resolve_file_symbols(&two);
    assert_eq!(r.symbols, vec!["__init__.py"], "an ambiguous name is kept");
    assert_eq!(
        r.ambiguous,
        vec![(
            "__init__.py".to_owned(),
            vec!["a/__init__.py".to_owned(), "b/__init__.py".to_owned()]
        )]
    );

    // A name that is not a file, and a path already on the stub, are kept.
    let kept = ic(&["dotproduct", "setup.py"], &["setup.py", "pkg/setup.py"]);
    let r = resolve_file_symbols(&kept);
    assert_eq!(r.symbols, vec!["dotproduct", "setup.py"]);
    assert!(r.resolved.is_empty() && r.ambiguous.is_empty());
}

/// REAL companion: the list stays open across a claim only inside one clause.
/// A file named after a sentence break, beside a claim, is not provided.
#[test]
fn a_file_named_after_a_clause_break_is_not_listed() {
    let i = ic(&["app/x.py", "setup.py"], &["app/x.py", "setup.py"]);
    let s = [
        statement(
            "p",
            &["I will provide `app/x.py`. It reads `config` and `setup.py`."],
            &[],
        ),
        statement("c", &[], &[]),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["app/x.py", "setup.py"]));
    assert!(
        f.contains(&HandshakeFinding::SymbolUnprovided {
            symbol: "setup.py".into()
        }),
        "{f:#?}"
    );
}

/// REAL companion: with two stub files of that name the handshake does not
/// guess; the symbol stays unprovided and holds.
#[test]
fn an_ambiguous_bare_file_name_still_holds() {
    let i = ic(&["__init__.py"], &["a/__init__.py", "b/__init__.py"]);
    let s = [
        statement("p", &["I will provide `a/__init__.py`."], &[]),
        statement("c", &[], &["I assume you provide `__init__.py`."]),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["a/__init__.py", "b/__init__.py"]));
    assert!(
        f.contains(&HandshakeFinding::SymbolUnprovided {
            symbol: "__init__.py".into()
        }),
        "{f:#?}"
    );
}

// ------------------------------------------------------------ narrow rules

/// A surface symbol that follows another in a sentence is a claim about it,
/// not a listed file: the provider's "`parse_config` returns `dict`"
/// disagrees with "`parse_config` returns `Config`", however the same party
/// describes `Config` elsewhere.
#[test]
fn a_symbol_claimed_about_another_is_not_met_by_its_provision_elsewhere() {
    let i = ic(&["parse_config", "Config"], &["app/config.py"]);
    let s = [
        statement(
            "p",
            &[
                "I will provide `parse_config` returns `dict`.",
                "I will provide `Config` is a dataclass.",
            ],
            &[],
        ),
        statement("c", &[], &["I assume `parse_config` returns `Config`."]),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["app/config.py"]));
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::Disagree { symbol, .. } if symbol == "parse_config")),
        "{f:#?}"
    );
}

/// A kind word that follows another anchor ("`rate` of type `f64`") is a
/// type claim about that anchor, even in a sentence about a file.
#[test]
fn a_type_given_to_an_item_inside_a_file_sentence_is_still_a_claim() {
    let i = ic(&["pkg/a.py"], &["pkg/a.py"]);
    let s = [
        statement(
            "p",
            &["I will provide `pkg/a.py` exports `rate` as `i64`."],
            &[],
        ),
        statement(
            "c",
            &[],
            &["I assume `pkg/a.py` exports `rate` of type `f64`."],
        ),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["pkg/a.py"]));
    assert!(
        matches!(&holding(&f)[..],
            [HandshakeFinding::Disagree { symbol, .. }] if symbol == "pkg/a.py"),
        "{f:#?}"
    );
}

/// The joins name a file's kind; a provider that gives the file another kind
/// in a different sentence still disagrees.
#[test]
fn file_kind_joins_and_a_contrary_kind_in_another_sentence() {
    let surface: BTreeSet<&str> = ["pkg/a.py", "b.toml"].into_iter().collect();
    assert_eq!(
        file_kinds(
            "`pkg/a.py` as type `Code`, `b.toml` (kind `Config`) and `x` of type `f64`",
            &surface,
            &BTreeSet::new()
        ),
        vec![
            ("pkg/a.py", "Code".to_owned()),
            ("b.toml", "Config".to_owned())
        ]
    );
    let i = ic(&["pkg/a.py", "b.toml"], &["pkg/a.py", "b.toml"]);
    let s = [
        statement(
            "p",
            &[
                "I will provide `pkg/a.py` with the code.",
                "I will provide `b.toml`; `pkg/a.py` of type `Config` is generated.",
            ],
            &[],
        ),
        statement(
            "c",
            &[],
            &["I assume you provide `pkg/a.py` as type `Code`."],
        ),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["pkg/a.py", "b.toml"]));
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::Disagree { symbol, .. } if symbol == "pkg/a.py")),
        "{f:#?}"
    );
}

/// A bare file name two stub files have names neither, even when only one
/// of them is on the surface: the provider's "`__init__.py`" does not
/// provide `a/__init__.py`.
#[test]
fn a_bare_name_two_stub_files_share_binds_to_neither() {
    let mut i = ic(&["a/__init__.py"], &["a/__init__.py", "b/__init__.py"]);
    i.symbols = vec!["a/__init__.py".into()];
    let s = [
        statement("p", &["I will provide `__init__.py`."], &[]),
        statement("c", &[], &["I assume you provide `a/__init__.py`."]),
    ];
    let f = compare_handshake(&i, &s, &writes_of(&["a/__init__.py", "b/__init__.py"]));
    assert!(
        f.contains(&HandshakeFinding::SymbolUnprovided {
            symbol: "a/__init__.py".into()
        }),
        "{f:#?}"
    );
}
