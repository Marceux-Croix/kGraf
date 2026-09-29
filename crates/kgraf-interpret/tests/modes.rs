//! Acceptance for the interpretation pass, against a fake model (no
//! network). Each test drives the effect it claims (test the mechanism, not the setting): a bounded input is
//! tested by giving it more than fits, a stored reading by counting model calls,
//! doneness by writing the code and the test results it is derived from.

mod support;

use kgraf_graph::WorkKind;
use kgraf_interpret::{
    CommandProvenance, Doneness, InterpretError, Mode, NoTestRuns, RecordedTests, prepare,
};
use serde_json::json;
use support::*;

#[tokio::test]
async fn greenfield_scopes_plans_and_seeds_the_graph_before_any_code() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    let root = dir.path();
    let store = store(root, db.path());

    // An empty directory and no instruction is nothing to plan: an error.
    assert!(matches!(
        prepare(&store, request()),
        Err(InterpretError::NothingToInterpret { .. })
    ));

    let model = FakeModel::new(|_| {
        answer(
            vec![
                item(
                    "cli",
                    &[],
                    json!({"paths": ["src/main.rs"], "symbols": ["main"]}),
                    "unstated",
                    json!([
                        {"text": "`todo add x` stores x", "check": {"kind": "test", "name": "adds_an_item"}},
                        {"text": "the help text reads well", "check": {"kind": "manual"}, "needs": "a person to read it"},
                    ]),
                ),
                item(
                    "store",
                    &["cli"],
                    json!({"symbols": ["TodoStore"]}),
                    "unstated",
                    json!([{"text": "cargo test passes", "check": {"kind": "command", "command": "cargo test"}}]),
                ),
            ],
            json!([{"command": "cargo test"}, {"command": "cargo clippy"}]),
            &["Where should the list be stored?"],
        )
    });
    let i = run(
        &store,
        instruction("build me a todo CLI in Rust"),
        &model,
        &NoTestRuns,
    )
    .await
    .expect("interpret");

    assert_eq!(i.mode(), Mode::Greenfield);
    assert!(model.last_brief().contains("The directory is empty"));
    assert!(model.last_brief().contains("build me a todo CLI in Rust"));
    assert_eq!(i.first_action(), Some("cli"));
    let flags = i.unverifiable();
    assert_eq!(flags.len(), 2, "{flags:?}");
    assert!(flags.iter().any(|f| f.needs == "a person to read it"));
    assert!(flags.iter().any(|f| f.reason.contains("cargo test")));
    let reading = i.reading();
    assert!(reading.contains("Where should the list be stored?"));
    assert!(reading.contains("proposed"));
    assert_eq!(i.proposed_commands(), ["cargo test", "cargo clippy"]);

    // Accepting one proposed command by name makes it a gate; the other stays a
    // proposal. Approval is bound to the plan id.
    let id = i.plan_id().to_owned();
    let approved = i.approve(&id, &["cargo test".into()]).expect("approve");
    assert_eq!(approved.validation_commands(), ["cargo test".to_owned()]);
    assert!(
        approved.items()[1]
            .acceptance
            .contains("[check: run `cargo test`]")
    );
    assert!(
        approved.items()[0]
            .acceptance
            .contains("cannot be checked automatically")
    );
    assert_eq!(
        approved.original_instruction(),
        "build me a todo CLI in Rust"
    );
    let seeded = approved.seed(&store, 1).expect("seed");
    assert!(seeded.adopted.is_empty(), "nothing was presumed");
    let write = seeded.write;
    assert_eq!(write.inserted, 4, "plan, component, two items");
    assert_eq!(write.deps_added, 1);
    let nodes = store.work_nodes(approved.plan_id()).expect("nodes");
    assert_eq!(nodes[0].kind, WorkKind::Plan);
    assert_eq!(nodes.iter().filter(|n| n.kind == WorkKind::Item).count(), 2);
    // Seeded before any code: the directory still holds nothing.
    assert_eq!(std::fs::read_dir(root).expect("read").count(), 0);
}

#[tokio::test]
async fn an_existing_project_is_read_back_with_discovered_and_invented_commands_told_apart() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    let root = dir.path();
    // A format with no parser rule beyond plain text: a spec as `.adoc`, rules as
    // `.txt`, a build file as TOML.
    write(
        root,
        "SPEC.adoc",
        "= Widget service\n\nThe service exposes `render_widget`. Before merging, run `just verify`.\n",
    );
    write(root, "RULES.txt", "No network access at build time.\n");
    write(root, "Cargo.toml", "[package]\nname = \"widgets\"\n");
    let store = store(root, db.path());
    let model = FakeModel::new(|_| {
        answer(
            vec![item(
                "render",
                &[],
                json!({"symbols": ["render_widget"], "tests": ["renders_a_widget"]}),
                "not_started",
                json!([
                    {"text": "verification passes", "check": {"kind": "command", "command": "just  verify"}},
                    {"text": "lint is clean", "check": {"kind": "command", "command": "make lint"}},
                    {"text": "fast enough"},
                ]),
            )],
            json!([{"command": "just verify", "source": "SPEC.adoc"}, {"command": "make lint", "source": "SPEC.adoc"}]),
            &[],
        )
    });
    let i = run(&store, request(), &model, &NoTestRuns)
        .await
        .expect("interpret");

    assert_eq!(i.mode(), Mode::Existing);
    let brief = model.last_brief();
    assert!(
        brief.contains("None was given"),
        "no instruction: the documents lead"
    );
    assert!(brief.contains("render_widget") && brief.contains("No network access"));
    assert!(brief.contains("<document source=\"SPEC.adoc\">"));
    let c = i.commands();
    assert_eq!(c[0].command, "just verify");
    assert!(
        matches!(&c[0].provenance, CommandProvenance::Discovered { path, .. } if path == "SPEC.adoc")
    );
    // The model's claimed source does not make `make lint` discovered.
    assert_eq!(c[1].command, "make lint");
    assert_eq!(c[1].provenance, CommandProvenance::Proposed);
    let flags = i.unverifiable();
    assert_eq!(flags.len(), 2, "{flags:?}");
    assert!(
        flags
            .iter()
            .any(|f| f.criterion == "fast enough" && f.reason == "it names no check")
    );
    assert_eq!(i.states()[0].doneness, Doneness::NotStarted);
    assert!(i.discrepancies().is_empty());

    // In an existing project too, approval does not promote a proposal nobody
    // accepted: the gates are the discovered command only.
    let id = i.plan_id().to_owned();
    let approved = i.clone().approve(&id, &[]).expect("approve");
    assert_eq!(approved.validation_commands(), ["just verify".to_owned()]);
    assert!(matches!(
        i.approve(&id, &["rm -rf /".into()]),
        Err(InterpretError::UnknownCommand { .. })
    ));

    // A correction is pinned verbatim and is a different reading (plan id).
    let mut corrected = request();
    corrected.corrections = vec!["The service is a library, not a server.".into()];
    let corrected = run(&store, corrected, &model, &NoTestRuns)
        .await
        .expect("interpret");
    assert_eq!(model.calls(), 2);
    assert!(
        model
            .last_brief()
            .contains("The service is a library, not a server.")
    );
    assert_ne!(corrected.plan_id(), id);
}

fn resume_repo(root: &std::path::Path) {
    write(
        root,
        "ROADMAP.md",
        "# Roadmap\n\n## Parser\nStatus: Complete. `parse_config` reads the file.\n\n\
         ## Exporter\nStatus: Complete. `export_csv` writes CSV.\n\n## Report\nNot started.\n",
    );
    write(
        root,
        "src/parser.rs",
        "pub fn parse_config() -> u32 { 1 }\n\n#[test]\nfn parses_a_config() { assert_eq!(parse_config(), 1); }\n",
    );
}

fn resume_answer() -> String {
    answer(
        vec![
            item(
                "parser",
                &[],
                json!({"paths": ["src/parser.rs"], "symbols": ["parse_config"], "tests": ["parses_a_config"]}),
                "complete",
                json!([{"text": "config parses", "check": {"kind": "test", "name": "parses_a_config"}}]),
            ),
            item(
                "exporter",
                &[],
                json!({"symbols": ["export_csv"], "tests": ["exports_csv"]}),
                "complete",
                json!([{"text": "csv written", "check": {"kind": "test", "name": "exports_csv"}}]),
            ),
            item(
                "report",
                &["parser"],
                json!({"symbols": ["build_report"]}),
                "not_started",
                json!([{"text": "report builds", "check": {"kind": "symbol", "name": "build_report"}}]),
            ),
        ],
        json!([]),
        &[],
    )
}

#[tokio::test]
async fn resume_derives_where_the_project_is_and_does_not_redo_finished_work() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    let root = dir.path();
    resume_repo(root);
    let store = store(root, db.path());
    let model = FakeModel::new(|_| resume_answer());

    // Without test results, code that exists derives Partial, not Complete: the
    // documents' "Complete" is a claim, and a claim is not a result.
    let i = run(&store, request(), &model, &NoTestRuns)
        .await
        .expect("interpret");
    assert_eq!(i.mode(), Mode::Resume);
    let state =
        |i: &kgraf_interpret::Interpretation, id: &str| i.state(id).expect("state").doneness;
    assert_eq!(state(&i, "parser"), Doneness::Partial);
    assert_eq!(state(&i, "exporter"), Doneness::NotStarted);
    let detail = |id: &str| {
        i.discrepancies()
            .iter()
            .find(|d| d.item == id)
            .map(|d| d.detail.clone())
            .unwrap_or_default()
    };
    assert!(
        detail("parser").contains("tests have not passed"),
        "{:?}",
        i.discrepancies()
    );
    assert!(detail("exporter").contains("none of its code exists"));

    // With the parser's test recorded as passing it derives Complete; the answer
    // is the stored reading (same brief, same model), so no second call.
    let passed = RecordedTests([("parses_a_config".to_owned(), true)].into());
    let i = run(&store, request(), &model, &passed)
        .await
        .expect("interpret");
    assert_eq!(model.calls(), 1, "an unchanged corpus is not asked again");
    assert!(i.is_cached());
    assert_eq!(state(&i, "parser"), Doneness::Complete);
    assert_eq!(i.first_action(), Some("exporter"));

    let id = i.plan_id().to_owned();
    let approved = i.approve(&id, &[]).expect("approve");
    assert_eq!(approved.skipped(), ["parser".to_owned()]);
    let ids: Vec<&str> = approved.items().iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        ["exporter", "report"],
        "finished work is not seeded again"
    );
    assert!(approved.items()[1].depends_on.is_empty());
}

#[tokio::test]
async fn a_partial_item_is_seeded_to_finish_what_exists() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    let root = dir.path();
    resume_repo(root);
    let store = store(root, db.path());
    let model = FakeModel::new(|_| resume_answer());
    let i = run(&store, request(), &model, &NoTestRuns)
        .await
        .expect("interpret");
    let id = i.plan_id().to_owned();
    let approved = i.approve(&id, &[]).expect("approve");
    let parser = &approved.items()[0];
    assert_eq!(parser.id, "parser");
    assert!(
        parser
            .instruction
            .starts_with("Part of this work already exists")
    );
    assert!(parser.instruction.contains("parse_config"));
    assert!(parser.instruction.contains("parses_a_config"));
    assert_eq!(parser.slices, ["parse_config".to_owned()]);
    assert_eq!(approved.items()[2].depends_on, ["parser".to_owned()]);
}

/// Decision 0013: a constraint naming `forbidden_paths` demonstrably
/// binds. The item that names a write under the forbidden path is seeded with
/// that write struck, not merely told about the rule in prose; a second item
/// whose write is elsewhere is untouched, so the strike is scoped to the path,
/// not the whole plan.
#[tokio::test]
async fn a_forbidden_path_constraint_strikes_the_write_before_seeding() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    let root = dir.path();
    write(
        root,
        "RULES.md",
        "legacy/ is frozen; nothing may write there.\n",
    );
    let store = store(root, db.path());
    let model = FakeModel::new(|_| {
        json!({
            "reading": "A small project with a frozen legacy area.",
            "components": [],
            "constraints": [{
                "text": "legacy/ is frozen; nothing may write there.",
                "source": "RULES.md",
                "forbidden_paths": ["legacy/"],
            }],
            "validation_commands": [],
            "work_items": [
                {
                    "id": "touch-legacy",
                    "title": "touch-legacy",
                    "instruction": "do touch-legacy",
                    "depends_on": [],
                    "acceptance": [{"text": "x", "check": {"kind": "manual"}, "needs": "n"}],
                    "evidence": {},
                    // Case- and separator-different from the constraint on
                    // purpose: the strike compares paths the way a worker's
                    // write scope already does (`Legacy` folds to `legacy`).
                    "writes": ["Legacy/old.rs", "src/new.rs"],
                    "documents_claim": "unstated",
                    "sources": ["RULES.md"],
                },
                {
                    "id": "elsewhere",
                    "title": "elsewhere",
                    "instruction": "do elsewhere",
                    "depends_on": [],
                    "acceptance": [{"text": "x", "check": {"kind": "manual"}, "needs": "n"}],
                    "evidence": {},
                    "writes": ["src/other.rs"],
                    "documents_claim": "unstated",
                    "sources": ["RULES.md"],
                },
            ],
            "questions": [],
        })
        .to_string()
    });
    let i = run(&store, request(), &model, &NoTestRuns)
        .await
        .expect("interpret");
    let id = i.plan_id().to_owned();
    let approved = i.approve(&id, &[]).expect("approve");

    let touched = approved
        .items()
        .iter()
        .find(|it| it.id == "touch-legacy")
        .expect("item");
    assert_eq!(
        touched.writes,
        vec!["src/new.rs".to_owned()],
        "the legacy write is struck; the other write on the same item is not"
    );
    let elsewhere = approved
        .items()
        .iter()
        .find(|it| it.id == "elsewhere")
        .expect("item");
    assert_eq!(
        elsewhere.writes,
        vec!["src/other.rs".to_owned()],
        "an item with no forbidden write is untouched"
    );
    assert_eq!(
        approved.blocked_writes(),
        [kgraf_interpret::BlockedWrite {
            item: "touch-legacy".into(),
            path: "Legacy/old.rs".into(),
            constraint: "legacy/ is frozen; nothing may write there.".into(),
        }],
        "the strike is recorded, not silent (decision 0013)"
    );
}

#[tokio::test]
async fn the_input_is_bounded_and_what_did_not_fit_is_named() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    let root = dir.path();
    let para = "The pipeline reads records and writes summaries. ".repeat(60);
    for i in 0..40 {
        let body: String = (0..6)
            .map(|s| format!("## Section {s}\n\n{para}\n\n"))
            .collect();
        write(
            root,
            &format!("docs/doc{i:02}.md"),
            &format!("# Doc {i}\n\n{body}"),
        );
    }
    for i in 0..30 {
        write(
            root,
            &format!("src/m{i}.rs"),
            &format!("pub fn handler_{i}() {{ shared_util(); }}\npub fn shared_util() {{}}\n"),
        );
    }
    let store = store(root, db.path());
    let budget = 6_000;
    let model = FakeModel::new(|_| {
        answer(
            vec![item(
                "w1",
                &[],
                json!({}),
                "unstated",
                json!([{"text": "x"}]),
            )],
            json!([]),
            &[],
        )
    });
    let mut r = instruction("summarize the pipeline");
    r.budget_tokens = budget;
    let i = run(&store, r, &model, &NoTestRuns)
        .await
        .expect("interpret");
    let sent = model.last_brief();
    let sent_tokens = kgraf_core::estimate_tokens(&sent)
        + kgraf_core::estimate_tokens(kgraf_interpret::brief::SYSTEM_PROMPT);
    assert!(sent_tokens <= budget, "{sent_tokens} > {budget}");
    assert_eq!(i.brief().tokens, sent_tokens);
    assert!(i.brief().sections_included > 0 && i.brief().definitions_included > 0);
    assert_eq!(
        i.brief().sections_included + i.brief().omitted.len(),
        i.brief().sections_total
    );
    assert!(!i.brief().omitted.is_empty());
    assert!(sent.contains("were omitted to fit the input bound"));
    assert!(i.reading().contains("were not read (input bound)"));
    // No whole file: a document larger than a chunk arrives in parts.
    assert!(!sent.contains(&std::fs::read_to_string(root.join("docs/doc00.md")).expect("read")));

    // The pinned instruction is never truncated; one too large is an error.
    let mut huge = instruction(&"word ".repeat(10_000));
    huge.budget_tokens = budget;
    assert!(matches!(
        prepare(&store, huge),
        Err(InterpretError::PinnedOverBudget { .. })
    ));
}

#[tokio::test]
async fn a_bad_answer_is_an_error_and_is_not_recorded() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    let root = dir.path();
    write(root, "README.md", "# Thing\n\nIt does a thing.\n");
    let store = store(root, db.path());
    let bad = FakeModel::new(|_| "I would start with the parser.".into());
    assert!(matches!(
        run(&store, request(), &bad, &NoTestRuns).await,
        Err(InterpretError::BadAnswer { .. })
    ));
    let cyclic = FakeModel::new(|_| {
        answer(
            vec![
                item("a", &["b"], json!({}), "unstated", json!([{"text": "x"}])),
                item("b", &["a"], json!({}), "unstated", json!([{"text": "x"}])),
            ],
            json!([]),
            &[],
        )
    });
    assert!(matches!(
        run(&store, request(), &cyclic, &NoTestRuns).await,
        Err(InterpretError::InvalidPlan(_))
    ));
    let good = FakeModel::new(|_| {
        answer(
            vec![item(
                "a",
                &[],
                json!({}),
                "unstated",
                json!([{"text": "x"}]),
            )],
            json!([]),
            &[],
        )
    });
    run(&store, request(), &good, &NoTestRuns)
        .await
        .expect("interpret");
    assert_eq!(good.calls(), 1, "the refused answers were not recorded");

    // A plan of only questions cannot be approved.
    let scoping = FakeModel::new(|_| answer(vec![], json!([]), &["What is it for?"]));
    std::fs::write(root.join("README.md"), "# Thing\n\nChanged.\n").expect("write");
    let i = run(&store, request(), &scoping, &NoTestRuns)
        .await
        .expect("interpret");
    let id = i.plan_id().to_owned();
    assert!(matches!(
        i.approve(&id, &[]),
        Err(InterpretError::NothingToApprove { .. })
    ));
}

/// Readings are keyed by model too: the same brief read by another model is a
/// different plan id, and the stored answer of one is never served for the other.
#[tokio::test]
async fn a_reading_is_keyed_by_model_and_old_readings_are_evicted() {
    let dir = tempfile::tempdir().expect("dir");
    let db = tempfile::tempdir().expect("db");
    write(dir.path(), "README.md", "# Thing\n\nIt does a thing.\n");
    let store = store(dir.path(), db.path());
    let good = || {
        answer(
            vec![item(
                "a",
                &[],
                json!({}),
                "unstated",
                json!([{"text": "x"}]),
            )],
            json!([]),
            &[],
        )
    };
    let model = FakeModel::new(move |_| good());
    let first = run(&store, request(), &model, &NoTestRuns)
        .await
        .expect("interpret");
    let mut other = request();
    other.model = "other:model".into();
    // The asker must be the model the request names.
    let p = prepare(&store, other.clone()).expect("prepare");
    assert!(!p.is_cached(), "another model's reading is not this one's");
    let same = prepare(&store, request()).expect("prepare");
    assert!(same.is_cached());
    assert_ne!(p.cache_key(), same.cache_key());
    let _ = first;
    assert!(matches!(
        p.ask(&model).await,
        Err(InterpretError::ModelMismatch { .. })
    ));

    // Many corrections: only the newest KEEP_READINGS readings stay.
    for n in 0..kgraf_interpret::KEEP_READINGS + 3 {
        let mut r = request();
        r.corrections = vec![format!("correction {n}")];
        run(&store, r, &model, &NoTestRuns)
            .await
            .expect("interpret");
    }
    let kept = store
        .meta_keys(kgraf_interpret::pass::READING_PREFIX)
        .expect("keys");
    assert_eq!(kept.len(), kgraf_interpret::KEEP_READINGS);
}
