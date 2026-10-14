//! The delta rules (decision 0100, sections 3 and 4) on records built in
//! memory: ids, amend or supersede, follow-ups, the merged plan's checks,
//! the apply-time re-classification, application per contract state, the
//! permission table and the prompt's size.

use super::*;
use crate::coordination::ClosedContract;
use crate::run_state::RunStatus;
use kgraf_core::{AgentId, RunId};

fn spec(id: &str, write: &str, deps: &[&str]) -> ContractSpec {
    ContractSpec {
        id: id.into(),
        instruction: format!("do {id}"),
        acceptance: format!("{write} exists"),
        writes: vec![WriteTarget {
            path: write.into(),
            node: None,
        }],
        depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
        slices: Vec::new(),
        tools: Vec::new(),
        route: None,
        attempts: 1,
        task_kind: None,
        check: None,
    }
}

fn open(s: &ContractSpec) -> OpenContract {
    let mut c = crate::plan::contract_from_spec(
        s,
        s.id.clone(),
        "r1",
        Vec::new(),
        Vec::new(),
        s.depends_on.clone(),
    );
    c.slice_names.clone_from(&s.slices);
    c
}

/// A record with `verified` closed and verified, and `open` queued, each
/// with its plan spec kept.
fn record(verified: &[ContractSpec], open_specs: &[ContractSpec]) -> RunStateRecord {
    let mut r = RunStateRecord::new(RunId::new("r1"), "build it", 0);
    for s in verified {
        r.coordination
            .closed
            .push(ClosedContract::of(&open(s), Closure::Verified));
        r.verified_contracts.push(s.id.clone());
        r.plan_specs.insert(s.id.clone(), s.clone());
    }
    for s in open_specs {
        r.open_contracts.push(open(s));
        r.plan_specs.insert(s.id.clone(), s.clone());
    }
    r.planned = true;
    r.coordination.gates = crate::gates::plan_gates(&r.open_contracts, &BTreeMap::new());
    r
}

fn change(r: &RunStateRecord) -> RecordedChange {
    let mut c = RecordedChange::received(
        u32::try_from(r.changes.len() + 1).expect("n"),
        ChangeSource::Change,
        "a change",
        None,
        1,
        1,
    );
    c.snapshot = snapshot(r);
    c
}

fn rules<'a>(roster: &'a [String], mode: PermissionMode) -> DeltaRules<'a> {
    DeltaRules {
        roster,
        tools: &[],
        max_contracts: 32,
        bypass_git_files: false,
        mode,
        own_checks: None,
    }
}

fn roster() -> Vec<String> {
    vec!["alpha".to_owned()]
}

fn prep(
    r: &RunStateRecord,
    adds: Vec<ContractSpec>,
    changes: Vec<ChangeOp>,
    removes: Vec<RemoveOp>,
) -> Result<Prepared, NotApplied> {
    let roster = roster();
    prepare(
        r,
        &change(r),
        adds,
        changes,
        removes,
        &rules(&roster, PermissionMode::Bypass),
    )
}

fn op(id: &str, spec: ContractSpec) -> ChangeOp {
    ChangeOp {
        id: id.into(),
        spec,
        why: String::new(),
    }
}

#[test]
fn an_edit_with_no_changed_line_is_no_change_and_a_real_edit_is_its_lines() {
    assert_eq!(line_diff("a\nb\n", "a\nb\n"), None);
    assert_eq!(
        line_diff("a\nb", "a  \nb\n"),
        None,
        "whitespace and the last newline"
    );
    let d = line_diff("a\nb\nc\n", "a\nB\nc\nd\n").expect("a diff");
    assert_eq!(d, "-b\n+B\n+d\n");
}

/// Decision 0100, section 4: the 15 cells of the permission table.
#[test]
fn the_permission_table_asks_exactly_where_decision_0100_says() {
    use DeltaClass::*;
    use PermissionMode::*;
    let want = [
        (Manual, [true, true, true]),
        (AcceptEdits, [false, true, true]),
        (Auto, [false, false, true]),
        (OrchestratorDecides, [false, false, false]),
        (Bypass, [false, false, false]),
    ];
    let mut cells = 0;
    for (mode, asks) in want {
        for (class, ask) in [Additive, Reshaping, Cutting].into_iter().zip(asks) {
            assert_eq!(mode_asks(mode, class), ask, "{mode:?} x {class:?}");
            // A change the user wrote is the yes for additive and reshaping
            // changes in every mode; a cutting one follows the mode.
            let with_file = if class == Cutting { ask } else { false };
            assert_eq!(
                needs_yes(mode, class, true),
                with_file,
                "{mode:?} x {class:?} file"
            );
            assert_eq!(needs_yes(mode, class, false), ask);
            cells += 1;
        }
    }
    assert_eq!(cells, 15);
}

#[test]
fn follow_ups_are_numbered_from_the_root_id() {
    let taken: BTreeSet<String> = ["c3".into(), "c3.f1".into()].into();
    assert_eq!(followup_root("c3.f1"), "c3");
    assert_eq!(followup_root("c3.f1.f2"), "c3");
    assert_eq!(followup_root("c3.fix"), "c3.fix");
    assert_eq!(next_followup("c3", &taken), "c3.f2");
    assert_eq!(next_followup("c3.f1", &taken), "c3.f2", "not c3.f1.f1");
}

#[test]
fn text_added_at_the_ends_amends_and_any_other_change_supersedes() {
    let old = spec("c1", "a.rs", &["c0"]);
    let mut amended = old.clone();
    amended.instruction.push_str(" and log it");
    amended.acceptance.push_str("; logged");
    assert!(is_amendment(&old, &amended));
    for change in [
        |s: &mut ContractSpec| {
            s.writes.push(WriteTarget {
                path: "b.rs".into(),
                node: None,
            })
        },
        |s: &mut ContractSpec| s.depends_on.clear(),
        |s: &mut ContractSpec| s.route = Some("alpha".into()),
        |s: &mut ContractSpec| s.check = Some("cargo test".into()),
        |s: &mut ContractSpec| s.instruction = "rewritten".into(),
    ] {
        let mut s = amended.clone();
        change(&mut s);
        assert!(!is_amendment(&old, &s), "{s:?}");
    }
}

/// Rule 1: added ids become `d<n>.<id>`, dependencies among them follow,
/// and an id already taken is refused.
#[test]
fn added_ids_take_the_change_prefix_and_a_collision_is_refused() {
    let r = record(&[spec("c1", "a.rs", &[])], &[spec("c2", "b.rs", &[])]);
    let p = prep(
        &r,
        vec![spec("x", "x.rs", &["c1"]), spec("y", "y.rs", &["x", "c2"])],
        Vec::new(),
        Vec::new(),
    )
    .expect("valid");
    let mut copy = r.clone();
    let done = apply(&mut copy, &p, "r1", false, 1);
    assert_eq!(done.added, vec!["d1.x".to_owned(), "d1.y".to_owned()]);
    let y = copy.contract("d1.y").expect("y");
    assert_eq!(y.depends_on, vec!["d1.x".to_owned(), "c2".to_owned()]);
    // A record that already holds `d1.x` (an earlier change's add).
    let mut taken = r.clone();
    taken
        .plan_specs
        .insert("d1.x".into(), spec("d1.x", "q.rs", &[]));
    let refused = prep(&taken, vec![spec("x", "x.rs", &[])], Vec::new(), Vec::new());
    assert!(matches!(refused, Err(NotApplied::Refused(why)) if why.contains("d1.x")));
}

/// Rule 3: a change on verified work is a follow-up that depends on it,
/// every live dependent gains a dependency on the follow-up, and a removal
/// of verified work is refused.
#[test]
fn a_change_on_verified_work_is_a_follow_up_and_its_removal_is_refused() {
    let r = record(&[spec("c1", "a.rs", &[])], &[spec("c2", "b.rs", &["c1"])]);
    let mut amended = spec("c1", "a.rs", &[]);
    amended.instruction.push_str(" with a log line");
    let p = prep(&r, Vec::new(), vec![op("c1", amended)], Vec::new()).expect("valid");
    assert_eq!(p.class, DeltaClass::Additive);
    let mut copy = r.clone();
    let done = apply(&mut copy, &p, "r1", false, 1);
    assert_eq!(done.followups, vec![("c1".to_owned(), "c1.f1".to_owned())]);
    let f = copy.contract("c1.f1").expect("follow-up");
    assert_eq!(f.depends_on, vec!["c1".to_owned()]);
    let c2 = copy.contract("c2").expect("dependent");
    assert_eq!(c2.depends_on, vec!["c1".to_owned(), "c1.f1".to_owned()]);
    let refused = prep(
        &r,
        Vec::new(),
        Vec::new(),
        vec![RemoveOp {
            id: "c1".into(),
            why: String::new(),
        }],
    );
    assert!(matches!(refused, Err(NotApplied::Refused(why)) if why.contains("verified")));
}

/// Rule 2 and the state table: an amendment of a running contract is a
/// redirect; a supersession or a removal of one stops its attempt and
/// closes it as replaced or removed, with dependents rewired.
#[test]
fn running_work_is_redirected_by_an_amendment_and_stopped_by_a_supersession() {
    let mut r = record(&[], &[spec("c1", "a.rs", &[]), spec("c2", "b.rs", &["c1"])]);
    r.open_contracts[0].workers = vec![AgentId::new("r1-c1-r0-a1")];
    let mut amended = spec("c1", "a.rs", &[]);
    amended.instruction.push_str(" quickly");
    let p = prep(&r, Vec::new(), vec![op("c1", amended)], Vec::new()).expect("amend");
    assert_eq!(p.class, DeltaClass::Additive);
    let mut copy = r.clone();
    let done = apply(&mut copy, &p, "r1", false, 1);
    assert_eq!(done.redirected, vec!["c1".to_owned()]);
    assert!(done.cancelled.is_empty());

    let widened = spec("c1", "z.rs", &[]);
    let p = prep(&r, Vec::new(), vec![op("c1", widened)], Vec::new()).expect("supersede");
    assert_eq!(p.class, DeltaClass::Cutting);
    let mut copy = r.clone();
    let done = apply(&mut copy, &p, "r1", false, 1);
    assert_eq!(done.superseded, vec![("c1".to_owned(), "d1.c1".to_owned())]);
    assert_eq!(
        done.cancelled,
        vec![("c1".to_owned(), vec!["r1-c1-r0-a1".to_owned()])]
    );
    let closed = copy
        .coordination
        .closed
        .iter()
        .find(|c| c.contract == "c1")
        .expect("closed");
    assert_eq!(closed.closure, Closure::Replaced);
    assert_eq!(
        copy.contract("c2").expect("c2").depends_on,
        vec!["d1.c1".to_owned()]
    );
    assert!(
        copy.contract("d1.c1")
            .expect("new revision")
            .workers
            .is_empty()
    );

    let p = prep(
        &r,
        Vec::new(),
        Vec::new(),
        vec![RemoveOp {
            id: "c1".into(),
            why: String::new(),
        }],
    )
    .expect("remove");
    let mut copy = r.clone();
    let done = apply(&mut copy, &p, "r1", false, 1);
    assert_eq!(done.cancelled.len(), 1);
    assert!(copy.contract("c2").expect("c2").depends_on.is_empty());
    let closed = copy
        .coordination
        .closed
        .iter()
        .find(|c| c.contract == "c1")
        .expect("closed");
    assert_eq!(closed.closure, Closure::Removed);
}

/// A queued contract's supersession or removal reshapes; a held one's
/// amendment clears the hold.
#[test]
fn queued_work_is_reshaped_and_a_held_amendment_clears_the_hold() {
    let mut r = record(&[], &[spec("c1", "a.rs", &[]), spec("c2", "b.rs", &[])]);
    r.open_contracts[1].status = ContractStatus::Held {
        reason: "re-grounding".into(),
    };
    let p = prep(
        &r,
        Vec::new(),
        vec![op("c1", spec("c1", "q.rs", &[]))],
        Vec::new(),
    )
    .expect("valid");
    assert_eq!(p.class, DeltaClass::Reshaping);
    let mut amended = spec("c2", "b.rs", &[]);
    amended.acceptance.push_str(" and builds");
    let p = prep(&r, Vec::new(), vec![op("c2", amended)], Vec::new()).expect("valid");
    let mut copy = r.clone();
    apply(&mut copy, &p, "r1", false, 1);
    assert_eq!(
        copy.contract("c2").expect("c2").status,
        ContractStatus::Planned
    );
}

/// Rule 5: an op whose target changed class since the snapshot is a
/// conflict, named; a target split by a handoff says so.
#[test]
fn a_target_that_moved_since_the_snapshot_is_a_conflict() {
    let r = record(&[], &[spec("c1", "a.rs", &[]), spec("c2", "b.rs", &[])]);
    let c = change(&r);
    let mut moved = r.clone();
    moved.open_contracts.retain(|o| o.contract != "c1");
    moved.verified_contracts.push("c1".into());
    let roster = roster();
    let got = prepare(
        &moved,
        &c,
        Vec::new(),
        vec![op("c1", spec("c1", "q.rs", &[]))],
        Vec::new(),
        &rules(&roster, PermissionMode::Bypass),
    );
    assert!(
        matches!(&got, Err(NotApplied::Conflict(list)) if list[0].contains("queued") && list[0].contains("verified")),
        "{got:?}"
    );
    let mut split = r.clone();
    split.open_contracts.retain(|o| o.contract != "c2");
    split.split_contracts.push("c2".into());
    split.coordination.closed.push(ClosedContract::of(
        &open(&spec("c2", "b.rs", &[])),
        Closure::Superseded,
    ));
    let got = prepare(
        &split,
        &c,
        Vec::new(),
        Vec::new(),
        vec![RemoveOp {
            id: "c2".into(),
            why: String::new(),
        }],
        &rules(&roster, PermissionMode::Bypass),
    );
    assert!(
        matches!(&got, Err(NotApplied::Conflict(list)) if list[0].contains("split by a handoff")),
        "{got:?}"
    );
}

/// Rule 4 as corrected: the merged plan (open and added) is validated with
/// closed ids external; the contract cap counts the change's adds only; a
/// route pin is judged against today's routes only for open and added
/// contracts (an assignment recorded at dispatch is not a pin).
#[test]
fn the_merged_plan_is_checked_with_closed_ids_external_and_the_cap_per_change() {
    let mut old = spec("c1", "a.rs", &[]);
    old.route = Some("gone".into());
    let mut r = record(&[old], &[spec("c2", "b.rs", &["c1"])]);
    r.open_contracts[0].route = Some("gone".into());
    r.open_contracts[0].route_pinned = false;
    assert!(
        prep(
            &r,
            vec![spec("x", "x.rs", &["c1", "c2"])],
            Vec::new(),
            Vec::new()
        )
        .is_ok()
    );
    let mut pinned = spec("y", "y.rs", &[]);
    pinned.route = Some("gone".into());
    assert!(matches!(
        prep(&r, vec![pinned], Vec::new(), Vec::new()),
        Err(NotApplied::Refused(why)) if why.contains("gone")
    ));
    assert!(matches!(
        prep(&r, vec![spec("z", "z.rs", &["nope"])], Vec::new(), Vec::new()),
        Err(NotApplied::Refused(why)) if why.contains("nope")
    ));
    let roster = roster();
    let mut small_cap = rules(&roster, PermissionMode::Bypass);
    small_cap.max_contracts = 1;
    let two = prepare(
        &r,
        &change(&r),
        vec![spec("p", "p.rs", &[]), spec("q", "q.rs", &[])],
        Vec::new(),
        Vec::new(),
        &small_cap,
    );
    assert!(matches!(two, Err(NotApplied::Refused(why)) if why.contains("at most 1")));
}

/// Section 4: under Orchestrator decides, a change that supersedes verified
/// work with nothing writing its files is refused outright.
#[test]
fn orchestrator_decides_refuses_dropping_verified_work_without_a_replacement() {
    let r = record(&[spec("c1", "a.rs", &[])], &[]);
    let roster = roster();
    let moved = prepare(
        &r,
        &change(&r),
        Vec::new(),
        vec![op("c1", spec("c1", "elsewhere.rs", &[]))],
        Vec::new(),
        &rules(&roster, PermissionMode::OrchestratorDecides),
    );
    assert!(matches!(moved, Err(NotApplied::Final(_))), "{moved:?}");
    let replaced = prepare(
        &r,
        &change(&r),
        Vec::new(),
        vec![op("c1", {
            let mut s = spec("c1", "a.rs", &[]);
            s.writes.push(WriteTarget {
                path: "b.rs".into(),
                node: None,
            });
            s
        })],
        Vec::new(),
        &rules(&roster, PermissionMode::OrchestratorDecides),
    )
    .expect("a replacement writes the file");
    assert_eq!(replaced.class, DeltaClass::Cutting);
}

/// A check that checks nothing is refused when own checks are on.
#[test]
fn a_vacuous_own_check_is_refused_with_own_checks_on() {
    let r = record(&[], &[]);
    let roster = roster();
    let settings = kgraf_tools::permissions::PermissionSettings {
        mode: PermissionMode::Bypass,
        ..Default::default()
    };
    let mut on = rules(&roster, PermissionMode::Bypass);
    on.own_checks = Some(&settings);
    let mut s = spec("x", "x.rs", &[]);
    s.check = Some("true".into());
    let got = prepare(
        &r,
        &change(&r),
        vec![s.clone()],
        Vec::new(),
        Vec::new(),
        &on,
    );
    assert!(matches!(got, Err(NotApplied::Refused(why)) if why.contains("checks nothing")));
    s.check = Some("cargo test -p x".into());
    assert!(prepare(&r, &change(&r), vec![s], Vec::new(), Vec::new(), &on).is_ok());
}

/// Gates over a changed contract reset: the root gate covers the new ids
/// and runs again.
#[test]
fn the_root_gate_covers_added_work_and_runs_again() {
    let mut r = record(&[], &[spec("c1", "a.rs", &[])]);
    r.coordination.gates[0].status = crate::gates::GateStatus::Passed {
        pinned: None,
        by: "cargo test".into(),
        at_ms: 1,
    };
    let p = prep(&r, vec![spec("x", "x.rs", &[])], Vec::new(), Vec::new()).expect("valid");
    let mut copy = r.clone();
    apply(&mut copy, &p, "r1", false, 1);
    let root = &copy.coordination.gates[0];
    assert!(root.covers.contains(&"d1.x".to_owned()));
    assert_eq!(root.status, crate::gates::GateStatus::Pending);
}

#[test]
fn the_three_answer_kinds_parse() {
    let d = parse_delta(
        r#"{"kind":"delta","add":[{"id":"x","instruction":"i","acceptance":"a","writes":["x.rs"]}],"change":null,"remove":[{"id":"c5"}]}"#,
    )
    .expect("delta");
    assert!(
        matches!(&d, DeltaAnswer::Delta { add, remove, .. } if add.len() == 1 && remove[0].id == "c5")
    );
    assert!(matches!(
        parse_delta(r#"{"kind":"no_change","answer":"already done"}"#).expect("no change"),
        DeltaAnswer::NoChange { .. }
    ));
    assert!(matches!(
        parse_delta(r#"{"kind":"replan_all","why":"different project"}"#).expect("replan"),
        DeltaAnswer::ReplanAll { .. }
    ));
    assert!(parse_delta(r#"{"kind":"rewrite"}"#).is_err());
}

#[test]
fn the_effective_instruction_is_the_original_and_each_applied_change() {
    let mut r = record(&[], &[]);
    assert_eq!(effective_instruction(&r), "build it");
    let mut c = RecordedChange::received(1, ChangeSource::Change, "add logs", None, 1, 1);
    c.effective_instruction = Some(instruction_after("build it", &c));
    c.state = ChangeState::Applied;
    r.changes.push(c);
    let mut refused = RecordedChange::received(2, ChangeSource::Change, "nope", None, 2, 1);
    refused.state = ChangeState::Refused { reason: "x".into() };
    r.changes.push(refused);
    assert!(effective_instruction(&r).contains("add logs"));
    assert_eq!(
        instruction_log(&r).len(),
        2,
        "a refused change is not in the log"
    );
    let mut e = RecordedChange::received(3, ChangeSource::Edit, "-a\n+b\n", Some("b"), 3, 1);
    e.state = ChangeState::Applied;
    e.effective_instruction = Some(instruction_after("ignored", &e));
    r.changes.push(e);
    assert_eq!(effective_instruction(&r), "b");
}

/// The prompt stays bounded: closed contracts go in short form (id, the
/// first line of the instruction, writes, dependencies), so a run of 32
/// open and 64 closed contracts with long instructions sends a prompt
/// whose closed part carries no instruction body.
#[test]
fn the_prompt_is_bounded_at_32_open_and_64_closed_contracts() {
    let body = "\nmore detail line ".repeat(40);
    let closed: Vec<ContractSpec> = (0..64)
        .map(|i| {
            let mut s = spec(&format!("v{i}"), &format!("src/v{i}.rs"), &[]);
            s.instruction = format!("verified step {i}{body}");
            s
        })
        .collect();
    let open_specs: Vec<ContractSpec> = (0..32)
        .map(|i| spec(&format!("o{i}"), &format!("src/o{i}.rs"), &[]))
        .collect();
    let r = record(&closed, &open_specs);
    let prompt = delta_prompt(&r, &change(&r), "");
    assert!(!prompt.contains("more detail line"), "a closed body leaked");
    assert!(prompt.contains("verified step 63"));
    assert!(prompt.len() < 24_000, "prompt is {} bytes", prompt.len());
    let full: usize = closed.iter().map(|s| s.instruction.len()).sum();
    assert!(full > 40_000, "the bodies left out are large: {full}");
}

/// A record with no kept specs marks closed contracts' dependencies as not
/// recorded, and their staleness after a supersession as unknown.
#[test]
fn an_older_record_says_dependencies_not_recorded_and_staleness_unknown() {
    let mut r = record(&[spec("c1", "a.rs", &[]), spec("c2", "b.rs", &["c1"])], &[]);
    r.plan_specs.remove("c2");
    let prompt = delta_prompt(&r, &change(&r), "");
    assert!(prompt.contains("dependencies not recorded"));
    let p = prep(
        &r,
        Vec::new(),
        vec![op("c1", spec("c1", "z.rs", &[]))],
        Vec::new(),
    )
    .expect("valid");
    let mut copy = r.clone();
    let done = apply(&mut copy, &p, "r1", false, 1);
    assert_eq!(done.unknown_staleness, vec!["c2".to_owned()]);
    assert_eq!(copy.status, RunStatus::Running);
}

#[test]
fn the_output_cap_scales_with_the_contracts_named() {
    let r = record(
        &[],
        &[
            spec("c1", "a.rs", &[]),
            spec("c2", "b.rs", &[]),
            spec("c3", "c.rs", &[]),
        ],
    );
    let mut c = change(&r);
    assert_eq!(output_cap(&r, &c), DELTA_MIN_TOKENS);
    c.text = "change c1, c2 and c3; not c10".into();
    assert_eq!(output_cap(&r, &c), 3 * DELTA_TOKENS_PER_CONTRACT);
}

/// Acceptance 8: the held-plan re-plan bound counts per change. Applying a
/// change sets the base to the re-plans taken so far.
#[test]
fn the_replan_bound_counts_from_the_newest_change() {
    let mut r = record(&[], &[spec("c1", "a.rs", &[])]);
    r.replans = 1;
    assert_eq!(r.replans_this_change(), 1);
    r.replan_base = 1;
    assert_eq!(
        r.replans_this_change(),
        0,
        "a new change has its own re-plan"
    );
    r.replans = 2;
    assert_eq!(r.replans_this_change(), 1);
}

fn landing(interrupted: bool) -> crate::land_record::LandingEntry {
    crate::land_record::LandingEntry {
        run_branch: "kgraf/r1".into(),
        candidate: "aaaa".into(),
        merge_commit: "bbbb".into(),
        target: "main".into(),
        target_old: "cccc".into(),
        gate: crate::land_record::LandGate::Untested {
            gate: "gate:root".into(),
            reason: "never ran".into(),
        },
        started_ms: 1,
        target_moved: false,
        checkout_interrupted: interrupted,
        include_changes: Vec::new(),
        programs_digest: None,
        fold: None,
        finished_ms: None,
    }
}

/// A pending land journal refuses a reopen, and a checkout the land stopped
/// part way is named; with no journal open nothing in the record refuses.
#[test]
fn an_open_land_journal_refuses_a_reopen_and_names_a_stopped_checkout() {
    let mut r = record(&[spec("c1", "a.rs", &[])], &[]);
    assert_eq!(reopen_refusal(&r), None);
    r.land.landing = Some(landing(false));
    let why = reopen_refusal(&r).expect("refused");
    assert!(why.contains("still in progress"), "{why}");
    r.land.landing = Some(landing(true));
    let why = reopen_refusal(&r).expect("refused");
    assert!(
        why.contains("stopped while git updated your checkout"),
        "{why}"
    );
    r.land.landing = None;
    r.land.landed.push(landing(false));
    assert_eq!(reopen_refusal(&r), None, "a finished land refuses nothing");
}

/// An own check counts only when it runs a test runner or a compiler, or
/// asserts over a file the contract writes.
#[test]
fn an_own_check_that_tests_nothing_is_vacuous() {
    let writes = vec!["src/a.rs".to_owned()];
    for vacuous in [
        "true",
        ":",
        "exit 0",
        "echo ok",
        "test -f src/a.rs",
        "test -e src/a.rs",
        "ls src",
        "cat src/a.rs",
        "grep -q x README.md",
        "echo a && ls",
    ] {
        assert!(
            crate::shortfall::own_check_vacuous(vacuous, &writes).is_some(),
            "{vacuous}"
        );
    }
    for real in [
        "cargo test -p a",
        "python3 -m pytest tests/test_a.py",
        "grep -q 'fn double' src/a.rs",
        "test -f src/a.rs && cargo build",
        "./node_modules/.bin/jest",
    ] {
        assert!(
            crate::shortfall::own_check_vacuous(real, &writes).is_none(),
            "{real}"
        );
    }
}

/// A contract whose own check is vacuous is no real check: its pass mark
/// does not verify the work, and it cannot overrule a vacuous verdict.
#[test]
fn a_vacuous_own_check_is_no_real_check() {
    let mut c = open(&spec("c1", "src/a.rs", &[]));
    c.contract_checks = vec!["test -f src/a.rs".into()];
    assert!(!crate::shortfall::own_checks_real(&c));
    let by = "true [contract_check_passed: test -f src/a.rs]";
    assert!(!crate::shortfall::own_check_counts(&c, by));
    let standing = crate::shortfall::accepted_standing(
        "c1",
        by,
        crate::shortfall::own_check_counts(&c, by),
        None,
    );
    assert!(standing.unverified.is_some(), "{standing:?}");
    c.contract_checks = vec!["cargo test -p a".into()];
    assert!(crate::shortfall::own_checks_real(&c));
    assert!(crate::shortfall::own_check_counts(
        &c,
        "true [contract_check_passed: cargo test -p a]"
    ));
    assert!(
        !crate::shortfall::own_check_counts(&c, "true"),
        "not run, not counted"
    );
}

/// The planner is told about `check` only with own checks on; off, the
/// prompt is the one it always was.
#[test]
fn the_own_check_note_is_added_only_with_own_checks_on() {
    assert_eq!(
        crate::plan::with_own_check_note(DELTA_SYSTEM, false),
        DELTA_SYSTEM
    );
    let on = crate::plan::with_own_check_note(DELTA_SYSTEM, true);
    assert!(on.starts_with(DELTA_SYSTEM) && on.contains("\"check\""));
}

/// An edit is logged as its whole edited text, not its (possibly cut) diff.
#[test]
fn the_instruction_log_carries_an_edits_whole_text() {
    let mut r = record(&[], &[]);
    let long = "line\n".repeat(4_000) + "THE-END";
    let diff = line_diff("x", &long).expect("a diff");
    assert!(!diff.contains("THE-END"), "the diff is cut for the log");
    let mut e = RecordedChange::received(1, ChangeSource::Edit, &diff, Some(&long), 1, 1);
    e.state = ChangeState::Applied;
    e.effective_instruction = Some(instruction_after("x", &e));
    r.changes.push(e);
    assert!(instruction_text(&r).contains("THE-END"));
    assert!(effective_instruction(&r).contains("THE-END"));
}

/// The per-change cap bounds follow-ups and new revisions too.
#[test]
fn the_per_change_cap_counts_follow_ups_and_new_revisions() {
    let r = record(&[spec("v1", "a.rs", &[])], &[spec("c1", "b.rs", &[])]);
    let roster = roster();
    let mut one = rules(&roster, PermissionMode::Bypass);
    one.max_contracts = 1;
    let mut amended = spec("v1", "a.rs", &[]);
    amended.instruction.push_str(" more");
    let got = prepare(
        &r,
        &change(&r),
        Vec::new(),
        vec![op("v1", amended), op("c1", spec("c1", "z.rs", &[]))],
        Vec::new(),
        &one,
    );
    assert!(
        matches!(&got, Err(NotApplied::Refused(why)) if why.contains("2 new contracts")),
        "{got:?}"
    );
}

/// An added contract may depend on verified work only: one depending on a
/// replaced or removed contract is refused by name.
#[test]
fn depending_on_replaced_or_removed_work_is_refused_by_name() {
    let mut r = record(&[spec("v1", "a.rs", &[])], &[]);
    r.coordination.closed.push(ClosedContract::of(
        &open(&spec("old", "o.rs", &[])),
        Closure::Replaced,
    ));
    let got = prep(
        &r,
        vec![spec("x", "x.rs", &["old"])],
        Vec::new(),
        Vec::new(),
    );
    assert!(
        matches!(&got, Err(NotApplied::Refused(why)) if why.contains("`old`") && why.contains("replaced")),
        "{got:?}"
    );
    assert!(prep(&r, vec![spec("x", "x.rs", &["v1"])], Vec::new(), Vec::new()).is_ok());
}

/// Outside Orchestrator decides, superseding verified work with nothing that
/// writes its files applies, and the loss is carried to be said.
#[test]
fn a_loss_of_verified_work_is_carried_in_every_other_mode() {
    let r = record(&[spec("c1", "a.rs", &[])], &[]);
    let p = prep(
        &r,
        Vec::new(),
        vec![op("c1", spec("c1", "elsewhere.rs", &[]))],
        Vec::new(),
    )
    .expect("bypass applies");
    assert!(p.lost.as_deref().is_some_and(|l| l.contains("a.rs")));
    let mut copy = r.clone();
    let done = apply(&mut copy, &p, "r1", false, 1);
    assert!(done.lost.is_some());
    let mut c = RecordedChange::received(1, ChangeSource::Change, "move it", None, 1, 1);
    c.state = ChangeState::Applied;
    c.applied = Some(done);
    copy.changes.push(c);
    assert_eq!(counts(&copy).verified_work_lost, 1);
}

/// An amendment of running work puts its redirect on the contract inside
/// the same application, so the change's one save carries it.
#[test]
fn an_amendments_redirect_is_written_with_the_change() {
    let mut r = record(&[], &[spec("c1", "a.rs", &[])]);
    r.open_contracts[0].workers = vec![AgentId::new("w1")];
    let mut amended = spec("c1", "a.rs", &[]);
    amended.instruction.push_str(" and log");
    let p = prep(&r, Vec::new(), vec![op("c1", amended)], Vec::new()).expect("amend");
    let mut copy = r.clone();
    apply(&mut copy, &p, "r1", false, 7);
    let c = copy.contract("c1").expect("c1");
    assert_eq!(c.redirects.len(), 1);
    assert!(c.redirects[0].contains("change 7") && c.redirects[0].contains("and log"));
    assert_eq!(redirect_row_id(7, "w1"), "change-7-w1");
}

/// With own checks off, a `check` in the answer is dropped, never stored.
#[test]
fn a_check_is_dropped_with_own_checks_off() {
    let r = record(&[], &[]);
    let mut s = spec("x", "x.rs", &[]);
    s.check = Some("cargo test".into());
    let p = prep(&r, vec![s], Vec::new(), Vec::new()).expect("valid");
    let mut copy = r.clone();
    apply(&mut copy, &p, "r1", false, 1);
    assert_eq!(copy.plan_specs["d1.x"].check, None);
    assert!(copy.contract("d1.x").expect("x").contract_checks.is_empty());
}
