//! Decision 0078: a recorded live handshake, replayed.
//!
//! The recorded run held both parties on seven findings. Its trace is committed
//! as a fixture, so the test reads the parties' statements and the recorded
//! findings verbatim from it, never from a restatement. Each recorded finding is pinned with its
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
/// and the reason.
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
        kind: crate::checks::InterfaceKind::Behavior,
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
        ..Default::default()
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

// ------------------------------------------------ recorded model statements
//
// Statements models gave in orchestrated runs, quoted verbatim. Each held its
// run on a finding the parties' words do not support; the REAL companions
// prove the relaxations keep true findings.

/// The `state_tax_rate` interface those runs held on.
fn tax_ic(id: &str, parties: &[&str]) -> InterfaceContract {
    InterfaceContract {
        kind: crate::checks::InterfaceKind::Behavior,
        id: id.into(),
        version: 1,
        parties: parties.iter().map(|p| (*p).to_owned()).collect(),
        symbols: vec!["state_tax_rate".into()],
        stub: vec![],
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

fn statement(
    party: &str,
    provide: &[&str],
    expect: &[&str],
    consume: &[&str],
) -> HandshakeStatement {
    let own = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect();
    HandshakeStatement {
        party: party.into(),
        provide: own(provide),
        expect: own(expect),
        consume: own(consume),
        touch: vec![],
        ..Default::default()
    }
}

const TAX_PROVIDES: &str = "I will provide `state_tax_rate` as a public function `fn state_tax_rate(state: &str) -> f64` in `tax_us/src/state_tax_rate.rs`, returning the `f64` value `0.08` for input CA, `0.0` for input OR, and `0.05` for every other `&str` input.";

/// Two statements that agree: the expecter backticks the state codes the
/// provider writes as plain words, and cites the interface by its label.
#[test]
fn an_expectation_citing_the_interface_and_the_providers_plain_words_agrees() {
    let ic = tax_ic("c3.c1.i1", &["c3.c1.c1", "c3.c1.c2"]);
    let f = compare_handshake(
        &ic,
        &[
            statement("c3.c1.c1", &[TAX_PROVIDES], &[], &[]),
            statement(
                "c3.c1.c2",
                &[
                    "I will provide the integration test file `tax_us/tests/state_tax_rate.rs` that includes `tax_us/src/state_tax_rate.rs` directly and asserts the required rates for `CA`, `OR`, and an unlisted state; I provide no contract symbols of my own.",
                ],
                &[
                    "I assume you provide the contract symbol `state_tax_rate` as a public, callable item defined in `tax_us/src/state_tax_rate.rs`.",
                    "I assume you provide `state_tax_rate` with a signature compatible with `fn state_tax_rate(state: &str) -> f64`, i.e. accepting a state code as `&str` and returning a numeric rate of type `f64`.",
                    "I assume you provide `state_tax_rate` behavior such that `CA` and `OR` return their required rates and any unlisted state returns the contract's default/unlisted rate, per contract `c3.c1.i1@v1`.",
                ],
                &[
                    "I assume `state_tax_rate` can be invoked from my test with arguments of type `&str` and its return value can be compared against `f64` literals.",
                ],
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(f.is_empty(), "{f:?}");
}

/// REAL companion: a state code the provider never states still disagrees.
#[test]
fn an_expected_value_the_provider_never_states_still_disagrees() {
    let ic = tax_ic("c3.c1.i1", &["c3.c1.c1", "c3.c1.c2"]);
    let f = compare_handshake(
        &ic,
        &[
            statement("c3.c1.c1", &[TAX_PROVIDES], &[], &[]),
            statement(
                "c3.c1.c2",
                &[],
                &["I assume you provide `state_tax_rate` returning `0.06` for `WA`."],
                &[],
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(
        matches!(&f[..], [HandshakeFinding::Disagree { symbol, .. }] if symbol == "state_tax_rate"),
        "{f:?}"
    );
}

/// One function split across two workers: the implementer provides it, the
/// test writer provides tests for it. Only one party provides the symbol.
#[test]
fn tests_for_a_symbol_are_not_a_second_provision_of_it() {
    let ic = tax_ic("c3.c1.i1", &["c3.c1.c1", "c3.c1.c2"]);
    let f = compare_handshake(
        &ic,
        &[
            statement(
                "c3.c1.c1",
                &[
                    "I will provide `state_tax_rate(state: &str) -> f64` in `tax_us/src/lib.rs`, returning the `f64` value `0.08` for `\"CA\"`, `0.0` for `\"OR\"`, and `0.05` for every other state code, while preserving all unrelated code in the file.",
                ],
                &[
                    "I assume you (party `c3.c1.c2`) provide the call sites that invoke `state_tax_rate`, passing a two-letter uppercase US state code as a `&str` (e.g. `\"CA\"`, `\"OR\"`) and consuming the returned `f64` rate; I assume no additional types, traits, or modules beyond the `state_tax_rate` symbol are expected from me.",
                ],
                &[
                    "I assume `state_tax_rate` is the only contract symbol and that its input arrives as a `&str` state code; I consume no other contract symbols from `c3.c1.c2`.",
                ],
            ),
            statement(
                "c3.c1.c2",
                &[
                    "I will provide integration tests for `state_tax_rate` in `tax_us/tests/state_tax_rate.rs`.",
                ],
                &["I assume `c3.c1.c1` provides the implementation of `state_tax_rate`."],
                &["I assume the tests can call `state_tax_rate`."],
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(f.is_empty(), "{f:?}");
}

/// REAL companion: two parties that each implement the symbol still double
/// provide it.
#[test]
fn two_implementations_of_one_symbol_are_still_double_provided() {
    let ic = tax_ic("c3.c1.i1", &["c3.c1.c1", "c3.c1.c2"]);
    let f = compare_handshake(
        &ic,
        &[
            statement("c3.c1.c1", &[TAX_PROVIDES], &[], &[]),
            statement(
                "c3.c1.c2",
                &["I will provide an implementation of `state_tax_rate` in `tax_us/src/lib.rs`."],
                &[],
                &[],
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(
        f.iter().any(
            |x| matches!(x, HandshakeFinding::DoubleProvided { symbol, .. } if symbol == "state_tax_rate")
        ),
        "{f:?}"
    );
}

/// A sentence stating that nothing is expected names no symbol because it
/// claims nothing; a sentence about the party's own code that only mentions
/// the surface symbol it calls makes no claim about that symbol.
#[test]
fn a_stated_absence_and_a_mention_after_own_code_are_not_findings() {
    let ic = tax_ic("c1.i1", &["c1.c1.c1", "c1.c2.c1"]);
    let f = compare_handshake(
        &ic,
        &[
            statement(
                "c1.c1.c1",
                &["I will provide `state_tax_rate` with type `fn(&str) -> f64`. "],
                &[
                    "I assume the other party does not provide any symbols required by this implementation.",
                ],
                &[],
            ),
            statement(
                "c1.c2.c1",
                &["I will provide `TaxUs2`'s implementation of `iface2::TaxRule`."],
                &[],
                &[
                    "I assume `tax_cents` receives an `order` with a `state` field and a `taxable_cents` value, and can call `state_tax_rate`.",
                ],
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(f.is_empty(), "{f:?}");
}

/// REAL companions: an unanchored assumption that states something is still
/// unanchored, and a claim written after the symbol it is about still counts.
#[test]
fn a_positive_unanchored_assumption_and_a_later_claim_are_still_findings() {
    let ic = tax_ic("c1.i1", &["c1.c1.c1", "c1.c2.c1"]);
    let f = compare_handshake(
        &ic,
        &[
            statement(
                "c1.c1.c1",
                &["I will provide `state_tax_rate` with type `fn(&str) -> f64`. "],
                &["I assume the other party provides a rate table."],
                &[],
            ),
            statement(
                "c1.c2.c1",
                &[],
                &[
                    "I assume `c1.c1.c1` provides `state_tax_rate` and the `iface2::TaxRule` interface.",
                ],
                &[],
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(
        f.iter().any(
            |x| matches!(x, HandshakeFinding::Unanchored { party, .. } if party == "c1.c1.c1")
        ),
        "{f:?}"
    );
    assert!(
        f.iter().any(
            |x| matches!(x, HandshakeFinding::Disagree { expecting, .. } if expecting == "c1.c2.c1")
        ),
        "{f:?}"
    );
}

// ------------------------------------- true disagreements the reading keeps

/// An anchor written before the symbol, in the same clause, is a claim about
/// it: a type the provider does not give still disagrees.
#[test]
fn a_type_written_before_the_symbol_still_disagrees() {
    let f = findings_for(&with_c2(
        "consume",
        "I assume `i32` is what `double` accepts and returns.",
    ));
    assert!(
        f.iter().any(|x| matches!(x,
            HandshakeFinding::Disagree { symbol, expecting, .. } if symbol == "double" && expecting == "c2")),
        "{f:?}"
    );
}

/// A negated assumption with no anchor still states something about the other
/// party; only a stated absence of symbols is set aside.
#[test]
fn a_negated_assumption_about_behavior_is_still_unanchored() {
    let ic = tax_ic("c1.i1", &["c1.c1.c1", "c1.c2.c1"]);
    let f = compare_handshake(
        &ic,
        &[
            statement(
                "c1.c1.c1",
                &["I will provide `state_tax_rate` with type `fn(&str) -> f64`."],
                &["I assume the other party does not mutate the input or panic."],
                &[],
            ),
            statement(
                "c1.c2.c1",
                &["I provide no contract symbols of my own."],
                &[],
                &["I assume `state_tax_rate` takes `&str`."],
            ),
        ],
        &BTreeMap::new(),
    );
    assert_eq!(
        f,
        vec![HandshakeFinding::Unanchored {
            party: "c1.c1.c1".into(),
            text: "I assume the other party does not mutate the input or panic.".into(),
        }],
        "the stated absence of symbols is set aside, the behavior claim is not"
    );
}

/// A type is matched only among the provider's anchors: a provider that writes
/// the expected type in its prose, about something else, still disagrees.
#[test]
fn a_type_named_only_in_the_providers_prose_still_disagrees() {
    let ic = InterfaceContract {
        symbols: vec!["double".into()],
        ..tax_ic("i1", &["p", "q"])
    };
    let f = compare_handshake(
        &ic,
        &[
            statement(
                "p",
                &[
                    "I will provide `double`, which accepts and returns `i32`; i64 callers must cast.",
                ],
                &[],
                &[],
            ),
            statement(
                "q",
                &[],
                &["I assume `double` accepts and returns `i64`."],
                &[],
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(
        matches!(&f[..], [HandshakeFinding::Disagree { symbol, .. }] if symbol == "double"),
        "{f:?}"
    );
}

/// A use word governs only the symbol it stands before: a provision that uses
/// another crate to implement the symbol still provides it, and a test writer's
/// provision is still set aside.
#[test]
fn a_use_word_about_another_anchor_does_not_drop_a_provision() {
    let ic = InterfaceContract {
        symbols: vec!["run".into()],
        ..tax_ic("i1", &["p", "q"])
    };
    let f = compare_handshake(
        &ic,
        &[
            statement("p", &["Using `clap`, I implement `run`."], &[], &[]),
            statement("q", &[], &["I assume `run` is provided."], &[]),
        ],
        &BTreeMap::new(),
    );
    assert!(f.is_empty(), "{f:?}");
    let f = compare_handshake(
        &ic,
        &[
            statement("p", &["I will provide `run`."], &[], &[]),
            statement(
                "q",
                &["I will provide tests in `tests/run.rs` that call `run`."],
                &[],
                &[],
            ),
        ],
        &BTreeMap::new(),
    );
    assert!(f.is_empty(), "{f:?}");
}

// ------------------------------------------ recorded plans held on wording
//
// Three handshakes from orchestrated runs, each fixture holding the registered
// interface, the parties' statements and the findings recorded then, verbatim.
// The plan itself is not in a trace: each party's declared writes are its
// touches (no `touch_outside_scope` was recorded, so each touch was declared),
// and the dependency order is the one the run's dependency waits recorded.

#[derive(serde::Deserialize)]
struct Replay {
    interface: InterfaceContract,
    handshake: Vec<HandshakeStatement>,
    recorded_findings: Vec<HandshakeFinding>,
    writes: BTreeMap<String, Vec<WriteTarget>>,
    depends_on: BTreeMap<String, Vec<String>>,
}

fn replay(json: &str) -> Replay {
    serde_json::from_str(json).expect("fixture")
}

fn replayed(r: &Replay) -> Vec<HandshakeFinding> {
    compare_handshake_ordered(
        &r.interface,
        &r.handshake,
        &r.writes,
        &ordered_pairs(&r.depends_on),
    )
}

fn holding(f: &[HandshakeFinding]) -> Vec<HandshakeFinding> {
    f.iter().filter(|x| x.holds()).cloned().collect()
}

/// One implementer and two test writers of `state_tax_rate`, all agreeing on
/// the stub's `fn state_tax_rate(&str) -> f64`. A test writer that says "I
/// will provide `state_tax_rate` integration-test coverage" writes no file of
/// the stub, so it provides nothing; an expectation the implementer's short
/// sentence does not restate, but the interface's own stub and test state, is
/// not a disagreement; an assumption naming no surface symbol is kept as a
/// warning, not a hold.
#[test]
fn test_writers_beside_one_implementer_do_not_hold() {
    let r = replay(include_str!(
        "../tests/fixtures/handshake/two-test-writers-and-an-implementer.json"
    ));
    assert!(
        r.recorded_findings.iter().any(|f| matches!(f,
            HandshakeFinding::DoubleProvided { parties, .. } if parties.len() == 2)),
        "the record held on a double provision"
    );
    let now = replayed(&r);
    assert_eq!(holding(&now), vec![], "{now:#?}");
    assert!(
        now.iter().any(|f| matches!(f,
            HandshakeFinding::OutsideSurface { symbol, .. } if symbol == "iface")),
        "the off-surface assumption is still recorded: {now:#?}"
    );
    assert!(
        now.iter().any(
            |f| matches!(f, HandshakeFinding::Unanchored { party, .. } if party == "c3.c2.c2")
        ),
        "{now:#?}"
    );
}

/// Two test writers of one test file, the second after the first, and one
/// implementer: nobody provides `accumulate` twice, and a file written by two
/// parties in dependency order is never written by both at once.
#[test]
fn serialized_test_writers_sharing_a_file_do_not_hold() {
    let r = replay(include_str!(
        "../tests/fixtures/handshake/tests-split-from-implementation.json"
    ));
    assert_eq!(r.recorded_findings.len(), 3, "the record held on three");
    let now = replayed(&r);
    assert_eq!(holding(&now), vec![], "{now:#?}");
    // The same statements with no order between the test writers: both may
    // run at once, so the shared file is still an overlap.
    let unordered =
        compare_handshake_ordered(&r.interface, &r.handshake, &r.writes, &BTreeSet::new());
    assert_eq!(
        holding(&unordered),
        vec![HandshakeFinding::TouchOverlap {
            path: "tests/accumulate.rs".into(),
            parties: vec!["c1.c1.c1".into(), "c1.c1.c2".into()],
        }],
        "{unordered:#?}"
    );
}

/// Three parties each write the one stub file and say they provide
/// `parse_duration`: a real double provision, still held, over exactly the
/// three writers. The fourth party writes nothing and provides nothing,
/// whatever its sentence names. The three run in dependency order, so their
/// shared file is not an overlap.
#[test]
fn three_implementers_of_one_symbol_still_hold() {
    let r = replay(include_str!(
        "../tests/fixtures/handshake/three-implementers-of-one-symbol.json"
    ));
    let now = replayed(&r);
    assert!(
        holding(&now).contains(&HandshakeFinding::DoubleProvided {
            symbol: "parse_duration".into(),
            parties: vec!["c1.c1.c1".into(), "c1.c2.c1".into(), "c1.c2.c2".into()],
        }),
        "{now:#?}"
    );
    assert!(
        !now.iter()
            .any(|f| matches!(f, HandshakeFinding::TouchOverlap { .. })),
        "{now:#?}"
    );
}

/// Which findings hold a contract: disagreements between parties do; a
/// sentence the comparator cannot bind to the surface does not.
#[test]
fn only_findings_between_parties_hold() {
    let warn = [
        HandshakeFinding::Unanchored {
            party: "p".into(),
            text: "t".into(),
        },
        HandshakeFinding::OutsideSurface {
            party: "p".into(),
            symbol: "s".into(),
            text: "t".into(),
        },
    ];
    assert!(warn.iter().all(|f| !f.holds()));
    let hold = [
        HandshakeFinding::DoubleProvided {
            symbol: "s".into(),
            parties: vec![],
        },
        HandshakeFinding::SymbolUnprovided { symbol: "s".into() },
        HandshakeFinding::Disagree {
            symbol: "s".into(),
            expecting: "a".into(),
            expects: "e".into(),
            providing: "b".into(),
            provides: "p".into(),
        },
        HandshakeFinding::TouchOverlap {
            path: "a.rs".into(),
            parties: vec![],
        },
    ];
    assert!(hold.iter().all(HandshakeFinding::holds));
}

/// A test writer assumes another test writer "does not change the
/// `parse_duration` interface", and the implementer provides it "using its
/// existing signature": the two agree. The negated assumption names a third
/// party, not the provider, so it is not read as the opposite of the
/// provider's sentence. The statements are the record's, verbatim; no
/// dependency wait was recorded, so no order is given.
#[test]
fn a_negated_assumption_about_a_third_party_does_not_contradict_the_provider() {
    let r = replay(include_str!(
        "../tests/fixtures/handshake/negated-assumption-about-a-third-party.json"
    ));
    assert!(
        r.recorded_findings.iter().any(|f| matches!(f,
            HandshakeFinding::Contradict { expecting, .. } if expecting == "c1.c2.c1")),
        "the record held on a contradiction"
    );
    let now = replayed(&r);
    assert_eq!(holding(&now), vec![], "{now:#?}");
    assert!(
        now.iter().any(
            |f| matches!(f, HandshakeFinding::Unanchored { party, .. } if party == "c1.c2.c2")
        ),
        "the unanchored provision is still a warning: {now:#?}"
    );
}

/// REAL companion: a negated expectation that names the provider, or names no
/// party, is still compared with the provision and still contradicts it.
#[test]
fn a_negated_assumption_about_the_provider_or_no_party_still_contradicts() {
    let ic = InterfaceContract {
        symbols: vec!["parse_duration".into()],
        ..tax_ic("c1.i1", &["c1.c1.c1", "c1.c2.c1", "c1.c2.c2"])
    };
    let provides = "I will provide `parse_duration`, which returns an error for empty input.";
    for expects in [
        "I assume `c1.c1.c1` provides `parse_duration` that never returns an error for empty input.",
        "I assume c1.c1.c1 provides `parse_duration` that never returns an error for empty input.",
        "I assume `parse_duration` never returns an error for empty input.",
    ] {
        let f = compare_handshake(
            &ic,
            &[
                statement("c1.c1.c1", &[provides], &[], &[]),
                statement("c1.c2.c1", &[], &[expects], &[]),
                statement(
                    "c1.c2.c2",
                    &["I provide no contract symbols of my own."],
                    &[],
                    &[],
                ),
            ],
            &BTreeMap::new(),
        );
        assert!(
            f.iter().any(|x| matches!(x,
                HandshakeFinding::Contradict { expecting, providing, .. }
                    if expecting == "c1.c2.c1" && providing == "c1.c1.c1")),
            "{expects}: {f:#?}"
        );
    }
}

// ------------------------------------------ a surface of files
//
// An interface over packaging files lists the files as its symbols: the
// files hold no item a consumer calls by name. A path anchor that is a
// surface symbol names it; every other path is still set aside.

/// The recorded handshake of a Python packaging interface (`pyproject.toml`
/// and `vectorops/__init__.py`, written by the producer, read by the build
/// step). Both parties named the files in backticks; setting every path aside
/// made each party `no_claims` and each file `symbol_unprovided`. Now the
/// producer provides both files and the consumer's expectations bind to
/// them. What still holds is real: the build step lists the producer's files
/// under `touch`, outside its declared writes. The fixture is the record's
/// interface, statements and findings, verbatim, with the plan's declared
/// writes and dependency.
#[test]
fn a_surface_of_files_binds_its_claims_and_only_real_findings_hold() {
    let r = replay(include_str!(
        "../tests/fixtures/handshake/files-as-contract-symbols.json"
    ));
    let no_claims = |f: &[HandshakeFinding]| {
        f.iter()
            .filter(|x| matches!(x, HandshakeFinding::NoClaims { .. }))
            .count()
    };
    let unprovided = |f: &[HandshakeFinding]| {
        f.iter()
            .filter(|x| matches!(x, HandshakeFinding::SymbolUnprovided { .. }))
            .count()
    };
    assert_eq!(
        (
            no_claims(&r.recorded_findings),
            unprovided(&r.recorded_findings)
        ),
        (2, 2),
        "the record held both parties on no claims and both files unprovided"
    );
    let now = replayed(&r);
    assert_eq!((no_claims(&now), unprovided(&now)), (0, 0), "{now:#?}");
    assert_eq!(
        holding(&now),
        vec![
            HandshakeFinding::TouchOutsideScope {
                party: "re1.c2_build".into(),
                path: "pyproject.toml".into(),
            },
            HandshakeFinding::TouchOutsideScope {
                party: "re1.c2_build".into(),
                path: "vectorops/__init__.py".into(),
            },
        ],
        "{now:#?}"
    );
}

/// The packaging interface with two files, written by `p`; `c` builds from
/// them.
fn files_ic() -> InterfaceContract {
    InterfaceContract {
        kind: crate::checks::InterfaceKind::Behavior,
        id: "i_pkg".into(),
        version: 1,
        parties: vec!["p".into(), "c".into()],
        symbols: vec!["pyproject.toml".into(), "pkg/__init__.py".into()],
        stub: vec![
            FileOverlay::new("pyproject.toml", "[project]\nname = 'wrong'\n"),
            FileOverlay::new("pkg/__init__.py", "import toml\n\ndef f():\n    return 0\n"),
        ],
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

fn files_writes(p: &[&str], c: &[&str]) -> BTreeMap<String, Vec<WriteTarget>> {
    let w = |l: &[&str]| {
        l.iter()
            .map(|path| WriteTarget {
                path: (*path).into(),
                node: None,
            })
            .collect()
    };
    [("p".to_owned(), w(p)), ("c".to_owned(), w(c))].into()
}

const FILES_PROVIDE: [&str; 2] = [
    "I will provide `pyproject.toml` with the package name and version.",
    "I will provide `./pkg/__init__.py` defining the package.",
];

/// Agreement on a surface of files passes; a file is provided by the party
/// that writes it, even when another stub file holds the file's extension as
/// a word (`import toml` in `pkg/__init__.py` does not make its writer the
/// provider of `pyproject.toml`).
#[test]
fn a_file_on_the_surface_is_provided_by_its_writer() {
    let f = compare_handshake(
        &files_ic(),
        &[
            statement("p", &FILES_PROVIDE, &[], &[]),
            statement(
                "c",
                &[],
                &["I assume you provide `/pyproject.toml` and `pkg/__init__.py`."],
                &["I consume `pyproject.toml` to build the wheel."],
            ),
        ],
        &files_writes(&["pyproject.toml", "pkg/__init__.py"], &["dist/pkg.whl"]),
    );
    assert_eq!(holding(&f), vec![], "{f:#?}");
    // One writer per file: `pyproject.toml` is provided by its own writer
    // when the other file's writer is someone else.
    let split = InterfaceContract {
        parties: vec!["p".into(), "q".into(), "c".into()],
        ..files_ic()
    };
    let mut writes = files_writes(&["pyproject.toml"], &[]);
    writes.insert(
        "q".into(),
        vec![WriteTarget {
            path: "pkg/__init__.py".into(),
            node: None,
        }],
    );
    let f = compare_handshake(
        &split,
        &[
            statement("p", &FILES_PROVIDE[..1], &[], &[]),
            statement("q", &FILES_PROVIDE[1..], &[], &[]),
            statement("c", &[], &["I assume you provide `pyproject.toml`."], &[]),
        ],
        &writes,
    );
    assert_eq!(holding(&f), vec![], "{f:#?}");
}

/// REAL companions: a surface of files still holds a party that names no
/// surface file, a second provider of one file, and a file nobody provides.
#[test]
fn a_surface_of_files_still_holds_real_disagreements() {
    let ic = files_ic();
    let writes = files_writes(&["pyproject.toml", "pkg/__init__.py"], &["dist/pkg.whl"]);
    // `c` names only a path off the surface: no claim, still held.
    let f = compare_handshake(
        &ic,
        &[
            statement("p", &FILES_PROVIDE, &[], &[]),
            statement("c", &[], &["I assume you provide `setup.cfg`."], &[]),
        ],
        &writes,
    );
    assert!(
        f.contains(&HandshakeFinding::NoClaims { party: "c".into() }),
        "{f:#?}"
    );
    // Both parties write and provide `pyproject.toml`: double provided.
    let both = files_writes(&["pyproject.toml", "pkg/__init__.py"], &["pyproject.toml"]);
    let f = compare_handshake_ordered(
        &ic,
        &[
            statement("p", &FILES_PROVIDE, &[], &[]),
            statement(
                "c",
                &["I will provide `pyproject.toml` with build settings."],
                &[],
                &[],
            ),
        ],
        &both,
        &ordered_pairs(&[("c".to_owned(), vec!["p".to_owned()])].into()),
    );
    assert!(
        holding(&f).iter().any(|x| matches!(x,
            HandshakeFinding::DoubleProvided { symbol, .. } if symbol == "pyproject.toml")),
        "{f:#?}"
    );
    // Nobody provides `pkg/__init__.py`: still unprovided.
    let f = compare_handshake(
        &ic,
        &[
            statement("p", &FILES_PROVIDE[..1], &[], &[]),
            statement("c", &[], &["I assume you provide `pyproject.toml`."], &[]),
        ],
        &writes,
    );
    assert!(
        f.contains(&HandshakeFinding::SymbolUnprovided {
            symbol: "pkg/__init__.py".into()
        }),
        "{f:#?}"
    );
}

/// A path anchor names a surface symbol only when the surface holds that
/// path; a leading `./` or `/` is not part of it.
#[test]
fn a_path_resolves_only_to_a_path_on_the_surface() {
    let surface: BTreeSet<&str> = ["pyproject.toml", "pkg/__init__.py", "double"].into();
    assert_eq!(
        resolve_path("pyproject.toml", &surface),
        Some("pyproject.toml")
    );
    assert_eq!(
        resolve_path("/pyproject.toml", &surface),
        Some("pyproject.toml")
    );
    assert_eq!(
        resolve_path("./pkg/__init__.py", &surface),
        Some("pkg/__init__.py")
    );
    assert_eq!(resolve_path("src/lib.rs", &surface), None);
    assert_eq!(resolve_path("other/pyproject.toml", &surface), None);
}

/// A touch is one file however it is spelled: `/pyproject.toml` from the
/// party that declares `pyproject.toml` is in scope, and two parties that may
/// run at once, touching `./a.py` and `/a.py`, overlap.
#[test]
fn a_touch_is_one_file_however_it_is_spelled() {
    let mut p = statement("p", &FILES_PROVIDE, &[], &[]);
    p.touch = vec!["/pyproject.toml".into(), "./pkg/__init__.py".into()];
    let mut c = statement("c", &[], &["I assume you provide `pyproject.toml`."], &[]);
    c.touch = vec!["dist/pkg.whl".into()];
    let writes = files_writes(&["pyproject.toml", "pkg/__init__.py"], &["dist/pkg.whl"]);
    let f = compare_handshake(&files_ic(), &[p.clone(), c.clone()], &writes);
    assert_eq!(holding(&f), vec![], "{f:#?}");
    c.touch = vec!["/pkg/__init__.py".into()];
    let both = files_writes(&["pyproject.toml", "pkg/__init__.py"], &["pkg/__init__.py"]);
    let f = compare_handshake(&files_ic(), &[p, c], &both);
    assert_eq!(
        holding(&f),
        vec![HandshakeFinding::TouchOverlap {
            path: "pkg/__init__.py".into(),
            parties: vec!["c".into(), "p".into()],
        }],
        "{f:#?}"
    );
}

/// The handshake prompt says what `touch` means: the paths the party will
/// write. A model given only the key `touch` listed files it reads, and the
/// comparator holds every touch outside the declared writes.
#[test]
fn the_handshake_prompt_defines_touch_as_writes() {
    assert!(
        HANDSHAKE_SYSTEM.contains("\"touch\" lists only the paths you will write"),
        "{HANDSHAKE_SYSTEM}"
    );
}
