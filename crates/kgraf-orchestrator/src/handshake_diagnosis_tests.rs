//! Decision 0078: the first live handshake, replayed.
//!
//! The run `run-p4checks-rust2` (evidence `2026-09-27-contract-check-cost.md`,
//! commit `0cbd67f`) held both parties on seven findings. Its trace is committed,
//! so the test reads the parties' statements and the recorded findings verbatim
//! from it, never from a restatement. Each recorded finding is pinned with its
//! classification: NOISE (same meaning, a spelling of it the comparator could not
//! read) must no longer be reported, REAL (the parties disagree) still must be.
//! For every class of noise relaxed, a synthetic REAL case proves the relaxation
//! does not hide a real disagreement.

use super::*;

const TRACE: &str = include_str!("../tests/fixtures/handshake/trace-run2.jsonl");

/// The trace event with this sequence number.
fn event(seq: u64) -> serde_json::Value {
    TRACE
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("trace line"))
        .find(|v| v["seq"] == seq)
        .unwrap_or_else(|| panic!("no seq {seq}"))
}

/// The registered interface `i1@v1` (seq 3).
fn live_ic() -> InterfaceContract {
    serde_json::from_value(event(3)["event"]["record"]["interface"].clone()).expect("interface")
}

/// The two parties' statements as the models gave them (seq 6).
fn live_statements() -> Vec<HandshakeStatement> {
    serde_json::from_value(event(6)["event"]["record"]["handshake"].clone()).expect("statements")
}

/// The findings the comparator recorded before 0078 (seq 6).
fn recorded_findings() -> Vec<HandshakeFinding> {
    serde_json::from_value(event(6)["event"]["record"]["findings"].clone()).expect("findings")
}

/// The parties' declared writes. The plan is not in the trace, but no
/// `touch_outside_scope` was recorded, so each party's four touches were in its
/// declared writes; that is the smallest set consistent with the record.
fn live_writes() -> BTreeMap<String, Vec<WriteTarget>> {
    let w = |p: &str| WriteTarget {
        path: p.into(),
        node: None,
    };
    [
        ("c1".to_owned(), vec![w("src/double.rs"), w("src/lib.rs")]),
        ("c2".to_owned(), vec![w("src/quad.rs"), w("src/lib.rs")]),
    ]
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Real,
    Noise,
}

/// The seven recorded findings, in recorded order, with their classification
/// and the reason (the evidence file quotes each case in full).
const CLASSIFIED: [(Class, &str); 7] = [
    (
        Class::Noise,
        "no_claims c2: c2 did claim about `double`, spelled `crate::double::double`",
    ),
    (
        Class::Noise,
        "outside_surface c2 `quadruple(..)`: c2's own provision, its brief, not the surface",
    ),
    (
        Class::Noise,
        "unprovided c1 `double` (expect): c1 names its own symbol; it assumes nothing of c2",
    ),
    (
        Class::Noise,
        "unprovided c1 `double` (consume): c1 restates its own symbol's type",
    ),
    (
        Class::Noise,
        "outside_surface c2 `c1`: a party id read as a symbol; the claim is `double`'s signature",
    ),
    (
        Class::Noise,
        "outside_surface c2 `crate::double::double`: `double`, qualified by its module path",
    ),
    (
        Class::Real,
        "touch_overlap src/lib.rs: c1 assumes c2 will not modify src/lib.rs; c2 will",
    ),
];

#[test]
fn the_record_holds_seven_findings_in_the_classified_order() {
    let recorded = recorded_findings();
    assert_eq!(recorded.len(), 7, "{recorded:#?}");
    let kind = |f: &HandshakeFinding| match f {
        HandshakeFinding::NoClaims { .. } => "no_claims",
        HandshakeFinding::OutsideSurface { .. } => "outside_surface",
        HandshakeFinding::Unprovided { .. } => "unprovided",
        HandshakeFinding::TouchOverlap { .. } => "touch_overlap",
        _ => "other",
    };
    for (f, (_, why)) in recorded.iter().zip(CLASSIFIED) {
        assert!(why.starts_with(kind(f)), "{f:?} is not `{why}`");
    }
}

#[test]
fn after_0078_every_noise_finding_passes_and_the_real_one_still_fails() {
    let now = compare_handshake(&live_ic(), &live_statements(), &live_writes());
    for (recorded, (class, why)) in recorded_findings().iter().zip(CLASSIFIED) {
        match class {
            Class::Noise => assert!(!now.contains(recorded), "NOISE still reported: {why}"),
            Class::Real => assert!(now.contains(recorded), "REAL no longer reported: {why}"),
        }
    }
    assert_eq!(
        now,
        vec![HandshakeFinding::TouchOverlap {
            path: "src/lib.rs".into(),
            parties: vec!["c1".into(), "c2".into()],
        }],
        "the live handshake still holds, on the one real finding"
    );
}

// -------------------------------------------------- synthetic REAL, per class

/// The live statements with one sentence of c2's replaced.
fn with_c2(field: &str, sentence: &str) -> Vec<HandshakeStatement> {
    let mut s = live_statements();
    let c2 = s.iter_mut().find(|s| s.party == "c2").expect("c2");
    let slot = match field {
        "expect" => &mut c2.expect,
        "consume" => &mut c2.consume,
        _ => panic!("field {field}"),
    };
    *slot = vec![sentence.to_owned()];
    s
}

fn findings_for(statements: &[HandshakeStatement]) -> Vec<HandshakeFinding> {
    compare_handshake(&live_ic(), statements, &live_writes())
}

/// Qualification (cases 1 and 6): a qualified name binds to the symbol, and the
/// claims then made about it are still compared.
#[test]
fn a_qualified_symbol_with_a_different_type_still_disagrees() {
    let f = findings_for(&with_c2(
        "consume",
        "I assume `crate::double::double` accepts and returns `i32`.",
    ));
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::Disagree { symbol, expecting, .. } if symbol == "double" && expecting == "c2")),
        "{f:?}"
    );
    // A qualified path whose last segment is another name is not `double`.
    let f = findings_for(&with_c2(
        "consume",
        "I assume `crate::double::twice` accepts and returns `i64`.",
    ));
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::OutsideSurface { symbol, .. } if symbol == "crate::double::twice")),
        "{f:?}"
    );
}

/// Qualified signatures (case 5): the signature is reduced to its types, and a
/// changed return type is a disagreement.
#[test]
fn a_qualified_signature_with_a_different_return_type_still_disagrees() {
    let f = findings_for(&with_c2(
        "expect",
        "I assume `c1` provides `crate::double::double(x: i64) -> i32` and its existing exports in `src/lib.rs`.",
    ));
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::Disagree { symbol, .. } if symbol == "double")),
        "{f:?}"
    );
}

/// Party ids (case 5): set aside, never read as a symbol, but an expectation of
/// something off the surface is still found.
#[test]
fn a_party_id_does_not_hide_an_expectation_off_the_surface() {
    let f = findings_for(&with_c2("expect", "I assume `c1` provides `triple`."));
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::OutsideSurface { party, symbol, .. } if party == "c2" && symbol == "triple")),
        "{f:?}"
    );
    let f = findings_for(&with_c2("expect", "I assume `c1` goes first."));
    assert!(
        f.iter()
            .any(|x| matches!(x, HandshakeFinding::Unanchored { party, .. } if party == "c2")),
        "{f:?}"
    );
}

/// Off-surface provisions (cases 1 and 2): no longer a finding, but a producer
/// that provides a misnamed function leaves the surface symbol unprovided, and a
/// party whose only anchors are off the surface still makes no claim.
#[test]
fn an_off_surface_provision_does_not_hide_a_missing_surface_symbol() {
    let mut s = live_statements();
    let c1 = s.iter_mut().find(|s| s.party == "c1").expect("c1");
    c1.provide = vec!["I will provide `twice(x: i64) -> i64`.".into()];
    c1.expect.clear();
    c1.consume.clear();
    let f = findings_for(&s);
    assert!(
        f.contains(&HandshakeFinding::SymbolUnprovided {
            symbol: "double".into()
        }),
        "{f:?}"
    );
    assert!(
        f.contains(&HandshakeFinding::NoClaims { party: "c1".into() }),
        "{f:?}"
    );
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::Unprovided { party, symbol, .. } if party == "c2" && symbol == "double")),
        "c2's expectation of `double` has no provider: {f:?}"
    );
}

/// Self-reference (cases 3 and 4): a party's sentence about its own symbol is not
/// compared, but a second provider is still a double provision, and an
/// expectation nobody provides is still unprovided.
#[test]
fn a_self_reference_does_not_hide_a_second_provider() {
    let mut s = live_statements();
    let c2 = s.iter_mut().find(|s| s.party == "c2").expect("c2");
    c2.provide
        .push("I will provide `double` returning `i64`.".into());
    let f = findings_for(&s);
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::DoubleProvided { symbol, .. } if symbol == "double")),
        "{f:?}"
    );
}

/// Subset (needed by cases 5 and 6): a provider may say more than the expecter
/// restates, but not less, and stated numbers must still agree.
#[test]
fn an_expectation_the_provider_does_not_state_still_fails() {
    let f = findings_for(&with_c2(
        "consume",
        "I assume `crate::double::double` returns `Option<i64>`.",
    ));
    assert!(
        f.iter()
            .any(|x| matches!(x, HandshakeFinding::Disagree { .. })),
        "{f:?}"
    );
    let f = findings_for(&with_c2(
        "consume",
        "I assume `crate::double::double` maps 21 to 43.",
    ));
    assert!(
        f.iter().any(
            |x| matches!(x, HandshakeFinding::Contradict { symbol, .. } if symbol == "double")
        ),
        "{f:?}"
    );
}

// ------------------------------------------------------ the reading, directly

#[test]
fn signatures_reduce_to_their_types() {
    let canon = |a: &str| signature(a).map(|s| (s.name, s.canonical));
    assert_eq!(
        canon("pub fn double(x: i64) -> i64"),
        Some(("double".into(), "fn(i64) -> i64".into()))
    );
    assert_eq!(
        canon("crate::double::double(x: i64) -> i64"),
        Some(("crate::double::double".into(), "fn(i64) -> i64".into()))
    );
    assert_eq!(
        canon("fn(i64) -> i64"),
        Some((String::new(), "fn(i64) -> i64".into()))
    );
    assert_eq!(
        canon("def parse_config(text: str) -> Config:"),
        Some(("parse_config".into(), "fn(str) -> Config".into()))
    );
    assert_eq!(
        canon("fn get(&self, k: HashMap<String, u8>) -> Option<u8>"),
        Some(("get".into(), "fn(HashMap<String, u8>) -> Option<u8>".into()))
    );
    for not_one in ["double(21) == 42", "#[cfg(test)] mod tests", "x * 2", "i64"] {
        assert!(signature(not_one).is_none(), "{not_one}");
    }
}

#[test]
fn paths_and_qualified_names_are_told_apart() {
    for p in ["src/lib.rs", "Cargo.toml", "config.py", "./a/b"] {
        assert!(is_path(p), "{p}");
    }
    for n in [
        "config.parse_config",
        "crate::double::double",
        "double",
        "x * 2",
    ] {
        assert!(!is_path(n), "{n}");
    }
    let surface: BTreeSet<&str> = ["double", "Config::new"].into();
    assert_eq!(resolve("crate::double::double", &surface), Some("double"));
    assert_eq!(resolve("config.Config::new", &surface), Some("Config::new"));
    assert_eq!(
        resolve("new", &surface),
        None,
        "a suffix of a symbol is not it"
    );
    assert_eq!(resolve("doubled", &surface), None);
}

/// A Python surface reads the same way: a `def` signature binds to the symbol
/// and its types are compared.
#[test]
fn a_python_signature_agrees_and_a_changed_return_type_disagrees() {
    let ic = InterfaceContract {
        id: "config".into(),
        version: 1,
        parties: vec!["prod".into(), "cons".into()],
        symbols: vec!["parse_config".into()],
        stub: vec![],
        wrong: vec![],
        reference: vec![],
        consumer: vec![],
        tests: vec![],
        compile: vec![],
        test: vec![],
        consulted: vec![],
        replaces: vec![],
    };
    let st = |party: &str, provide: &str, expect: &str| HandshakeStatement {
        party: party.into(),
        provide: (!provide.is_empty())
            .then(|| provide.to_owned())
            .into_iter()
            .collect(),
        expect: (!expect.is_empty())
            .then(|| expect.to_owned())
            .into_iter()
            .collect(),
        consume: vec![],
        touch: vec![],
    };
    let prod = st(
        "prod",
        "I will provide `def parse_config(text: str) -> Config:` in `config.py`",
        "",
    );
    let f = compare_handshake(
        &ic,
        &[
            prod.clone(),
            st(
                "cons",
                "",
                "I assume `prod` provides `config.parse_config` taking `str` and returning `Config`",
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(f.is_empty(), "{f:?}");
    let f = compare_handshake(
        &ic,
        &[
            prod,
            st(
                "cons",
                "",
                "I assume `parse_config` returns `Optional[Config]`",
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(
        matches!(&f[..], [HandshakeFinding::Disagree { symbol, .. }] if symbol == "parse_config"),
        "{f:?}"
    );
}
