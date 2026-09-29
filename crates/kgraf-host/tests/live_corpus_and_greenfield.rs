//! Live acceptance for the corpus reading and greenfield items (decision
//! 0013), against real provider routes: the self-test comparison, constraint
//! binding, and a greenfield plan carried through to dispatch.
//!
//! Ignored by default: it spends money. Keys come **only** from a `KEY=VALUE`
//! file named by `KGRAF_LIVE_ENV_FILE` (mirrors `live_sessions.rs`'s
//! `env_file`/`set_route_credentials`, duplicated here rather than shared
//! across two small test binaries): read into memory, set as
//! `KGRAF_KIMI_API_KEY` in this test process's own environment only, never
//! printed. A route whose key is absent is skipped.
//!
//! ```text
//! KGRAF_LIVE_CORPUS=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-host --test live_corpus_and_greenfield -- \
//!   --ignored --nocapture --test-threads=1
//! ```
use kgraf_core::RunId;
use kgraf_host::{CorpusRequest, Host, RunConfig, RunStart};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn prices_path() -> PathBuf {
    match std::env::var("KGRAF_LIVE_PRICES_FILE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prices.json"),
    }
}

/// This repository's own root: `CARGO_MANIFEST_DIR` is
/// `<repo>/crates/kgraf-host`, so the self-test reads two levels up.
fn this_repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("this repository's root resolves")
}

fn env_file() -> std::collections::BTreeMap<String, String> {
    let Ok(path) = std::env::var("KGRAF_LIVE_ENV_FILE") else {
        return Default::default();
    };
    let text = std::fs::read_to_string(&path).expect("KGRAF_LIVE_ENV_FILE is readable");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("export ").unwrap_or(l).split_once('='))
        .map(|(k, v)| {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            (k.trim().to_owned(), v.to_owned())
        })
        .collect()
}

/// Sets this test process's own `KGRAF_KIMI_API_KEY` from the `.env` file's
/// `HAUSTORIA_KIMI_API_KEY` (or its current name), never printed, never read
/// again from the file itself. Returns whether a key was found.
fn set_kimi_credential() -> bool {
    let file = env_file();
    let key = file
        .get("HAUSTORIA_KIMI_API_KEY")
        .or_else(|| file.get("KGRAF_KIMI_API_KEY"))
        .cloned();
    match key {
        Some(k) => {
            // SAFETY-in-spirit note: `std::env::set_var` inside this test's
            // own process only; the value is never printed or logged.
            unsafe { std::env::set_var("KGRAF_KIMI_API_KEY", k) };
            true
        }
        None => false,
    }
}

fn live() -> bool {
    std::env::var("KGRAF_LIVE_CORPUS").is_ok_and(|v| v == "1")
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

const ROUTE: &str = "kimi-anthropic:kimi-k3";

/// Acceptance item 1: "pointed at kGraf's own repository, whose
/// correct interpretation is known, the plan model it produces is compared
/// against the actual roadmap, decisions and standing rules." The comparison
/// itself is written up in the evidence file (a live model's prose does not
/// lend itself to a brittle automated assertion, per the existing readings in
/// `2026-09-26-phase7-live.md`); this test's job is to produce the reading and
/// record its mechanics: one ledgered call, a non-empty reading, and the
/// bounded-input budget actually bounding it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn self_test_reads_this_repository_against_its_own_roadmap() {
    if !live() {
        println!("KGRAF_LIVE_CORPUS is not 1; not run");
        return;
    }
    if !set_kimi_credential() {
        println!("SKIPPED: no Kimi key in KGRAF_LIVE_ENV_FILE");
        return;
    }
    let root = this_repository();
    println!("self-test root: {}", root.display());
    let mut config = RunConfig::new(root, vec![ROUTE.to_owned()], prices_path());
    // kimi-k3 spends a large share of its output budget on reasoning before the
    // JSON answer; 6,000 was consumed almost entirely by reasoning (5,997 of
    // 6,000) and cut the answer off (`InterpretError::Model`, "cut off at the
    // output limit"). 16,000 leaves headroom for both.
    config.planner_max_tokens = 16_000;
    let host = Host::open(config).expect("host opens");
    let intake = host
        .read_corpus(
            &RunId::new("self-test"),
            CorpusRequest {
                instruction: Some(
                    "Read this project as it stands today. State which roadmap phase is most \
                     actionable next, the binding constraints its standing rules and decisions \
                     place on all work, and at most 4 next work items."
                        .to_owned(),
                ),
                corrections: Vec::new(),
                // 0057's bound, tightened: enough to reach the roadmap's phase table and
                // the repository's standing rules, small enough to keep this call cheap.
                budget_tokens: Some(24_000),
            },
        )
        .await
        .expect("read_corpus (provider-level failure)");
    println!("charged calls: {:?}", intake.charged);
    let reading = intake.result.expect("interpretation succeeded");
    println!("=== plan id ===\n{}", reading.plan_id());
    println!("=== reading ===\n{}", reading.reading());
    println!("=== constraints ===");
    for c in &reading.answer().constraints {
        println!(
            "- {} (source: {:?}, forbidden_paths: {:?})",
            c.text, c.source, c.forbidden_paths
        );
    }
    println!("=== work items ===");
    for w in &reading.answer().work_items {
        println!("- {} :: {}", w.id, w.title);
    }
    let t = reading.timings();
    println!(
        "first action {} ms (index {} ms, brief {} ms, model {} ms)",
        t.first_action_ms, t.index_ms, t.brief_ms, t.model_ms
    );
    assert!(
        !reading.answer().reading.trim().is_empty(),
        "a reading was produced"
    );
    assert_eq!(intake.charged.len(), 1, "one ledgered interpretation call");
}

/// Acceptance item 2: "Constraints extracted from the corpus
/// demonstrably bind: a rule in the documents forbidding a behavior makes the
/// orchestrator unable to produce it" (decision 0013). A tiny scratch corpus
/// states a frozen path in prose; the instruction asks, among other things,
/// for a write inside it. Whatever the model actually returns is printed and
/// recorded (a live model's instruction-following is not this
/// mechanism's own contract, per the existing readings in
/// `2026-09-26-phase7-live.md`); when it does name the forbidden path, this
/// shows `ApprovedPlan::blocked_writes` actually strike it before the plan is
/// seeded, not merely echo the rule in prose.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn a_forbidden_path_constraint_binds_live() {
    if !live() {
        println!("KGRAF_LIVE_CORPUS is not 1; not run");
        return;
    }
    if !set_kimi_credential() {
        println!("SKIPPED: no Kimi key in KGRAF_LIVE_ENV_FILE");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("RULES.md"),
        "# Rules\n\n\
         `legacy/` is a frozen area of this project. Nothing may ever write to `legacy/`, \
         for any reason. Extract this as a constraint whose forbidden_paths names it.\n",
    )
    .expect("write RULES.md");
    std::fs::write(root.join("README.md"), "A tiny scratch project.\n").expect("write README.md");
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "scratch@example.invalid"]);
    git(&root, &["config", "user.name", "scratch"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "seed"]);

    let mut config = RunConfig::new(root, vec![ROUTE.to_owned()], prices_path());
    config.planner_max_tokens = 3_000;
    let host = Host::open(config).expect("host opens");
    let intake = host
        .read_corpus(
            &RunId::new("constraint-bind"),
            CorpusRequest {
                instruction: Some(
                    "Add a helper function in legacy/util.rs that reverses a string, and a \
                     second, unrelated helper in src/util.rs that adds two numbers. RULES.md is \
                     binding: read it and extract its rule as a constraint."
                        .to_owned(),
                ),
                corrections: Vec::new(),
                budget_tokens: None,
            },
        )
        .await
        .expect("read_corpus (provider-level failure)");
    println!("charged calls: {:?}", intake.charged);
    let reading = match intake.result {
        Ok(r) => r,
        Err(e) => {
            println!(
                "interpretation refused (a live model's schema fidelity, not this \
                       mechanism): {e}"
            );
            return;
        }
    };
    println!("=== reading ===\n{}", reading.reading());
    println!("=== constraints ===");
    for c in &reading.answer().constraints {
        println!(
            "- {} (source: {:?}, forbidden_paths: {:?})",
            c.text, c.source, c.forbidden_paths
        );
    }
    let id = reading.plan_id().to_owned();
    let approved = match reading.approve(&id, &[]) {
        Ok(a) => a,
        Err(e) => {
            println!("nothing to approve yet: {e}");
            return;
        }
    };
    println!("=== blocked writes ===");
    for b in approved.blocked_writes() {
        println!(
            "- item {}: write to {} struck ({})",
            b.item, b.path, b.constraint
        );
    }
    for item in approved.items() {
        println!("item {}: writes {:?}", item.id, item.writes);
    }
    if approved
        .blocked_writes()
        .iter()
        .any(|b| b.path.to_lowercase().contains("legacy"))
    {
        println!(
            "BOUND: the model named the rule's path and the write to it was struck before \
                   seeding."
        );
    } else {
        println!(
            "NOT DEMONSTRATED THIS RUN: the model did not name legacy/ as a forbidden \
                   path (or named no legacy/ write to strike); see the evidence file for what \
                   it did instead. The mechanism itself (`a_forbidden_path_constraint_strikes_\
                   the_write_before_seeding` in kgraf-interpret) is unit-tested and does not \
                   depend on this call's outcome."
        );
    }
}

/// Acceptance item 3: a greenfield plan carried through to dispatch,
/// against a real route. From an empty directory (`git init`, no commit) and
/// one instruction, exactly the prior evidence's greenfield reading
/// (`2026-09-26-phase7-live.md`), approved and built: the run's integration
/// branch now exists (the `ensure_integration_branch` fix) and at least one
/// worker is actually dispatched.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn a_greenfield_directory_reaches_dispatch_live() {
    if !live() {
        println!("KGRAF_LIVE_CORPUS is not 1; not run");
        return;
    }
    if !set_kimi_credential() {
        println!("SKIPPED: no Kimi key in KGRAF_LIVE_ENV_FILE");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "scratch@example.invalid"]);
    git(&root, &["config", "user.name", "scratch"]);
    let rev_parse = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .status()
        .expect("git");
    assert!(
        !rev_parse.success(),
        "the fixture must start with no commit"
    );

    let mut config = RunConfig::new(root.clone(), vec![ROUTE.to_owned()], prices_path());
    config.cap_task = Some(0.60);
    config.max_items = 2;
    config.max_agents = 2;
    config.steps = 4;
    config.max_tokens = 400;
    // kimi-k3 spends a large, variable share of `planner_max_tokens` on
    // reasoning before its JSON answer (the self-test above hit the same
    // ceiling at 6,000); this bounds both the reading call and the run's own
    // orchestrator calls, so it must cover the larger of the two.
    config.planner_max_tokens = 12_000;
    let host = Host::open(config).expect("host opens");
    let intake = host
        .read_corpus(
            &RunId::new("greenfield-live"),
            CorpusRequest {
                instruction: Some("A tiny Rust CLI that reverses each line of stdin.".to_owned()),
                corrections: Vec::new(),
                budget_tokens: None,
            },
        )
        .await
        .expect("read_corpus (provider-level failure)");
    println!("charged calls (reading): {:?}", intake.charged);
    let reading = intake.result.expect("a greenfield reading");
    println!("=== reading ===\n{}", reading.reading());
    let id = reading.plan_id().to_owned();
    let approved = reading
        .approve(&id, &[])
        .expect("approve (a scoping-only reading refuses)");
    println!(
        "plan {} approved: {} item(s)",
        approved.plan_id(),
        approved.items().len()
    );

    let startup = host
        .startup(&mut |n| println!("notice: {n:?}"))
        .expect("startup");
    let run_id = RunId::new(format!(
        "run-greenfield-live-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let prepared = host
        .build(&startup, run_id.clone(), Some(&approved), &mut |n| {
            println!("notice: {n:?}")
        })
        .await
        .expect("build");
    println!(
        "integration branch: {} (root {})",
        prepared.summary().integration,
        root.display()
    );

    let outcome = tokio::time::timeout(
        Duration::from_secs(300),
        prepared.run(RunStart::Plan(Box::new(approved))),
    )
    .await
    .expect("run finished in time")
    .expect("run completed (or held/failed, both reported)");

    println!("status     {:?}", outcome.report.status);
    println!("dispatched {}", outcome.report.counters.dispatches);
    println!("verified   {}", outcome.report.verified.join(", "));
    println!("spent      {}", outcome.report.spent);
    assert!(
        outcome.report.counters.dispatches >= 1,
        "the greenfield plan reached dispatch: {:?}",
        outcome.report.counters
    );
}
