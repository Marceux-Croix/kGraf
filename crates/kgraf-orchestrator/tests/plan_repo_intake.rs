//! Plan intake against the repository: a plan that leaves a named scaffold
//! file unwritten, or whose acceptance runs cargo in a workspace before its
//! members exist (in any form: `--workspace`, `-p`, ...), is refused with a reason the planner can act on and is asked again.
//! A cargo workspace refusal with a mechanical remedy (an edge, a manifest
//! moved to the contract that needs it, a minimal member source) is repaired
//! instead, with a trace record and a notice per change, and no ask is spent.
//!
//! Observed cases: a plan gave the clearance discount to a contract
//! that wrote only `catalog/src/lib.rs`, leaving the stub
//! `discount_clearance/src/lib.rs` to no contract, and its worker was held at
//! the file; a plan for a new workspace wrote `Cargo.toml` with members `wcl-core` and `wcl` in
//! one contract and `wcl` in another, with `cargo test --workspace` accepting
//! the first, then `cargo test -p wcl-core` once that was refused. The
//! planner, grounder and workers are fake providers.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    DirWorkspaces, OpenContract, Orchestrator, TraceEvent, TraceLog, Workspaces,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;

/// Plain directories for worktrees over a repository root the plan is
/// checked against.
struct Rooted {
    dirs: DirWorkspaces,
    root: PathBuf,
}

#[async_trait::async_trait]
impl Workspaces for Rooted {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &kgraf_core::AgentId,
    ) -> Result<PathBuf, kgraf_orchestrator::PrepareFailure> {
        self.dirs.prepare(contract, worker).await
    }

    async fn release(&self, workspace: &Path, keep: bool) {
        self.dirs.release(workspace, keep).await
    }

    fn repo_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }
}

const STUB: &str = "use iface2::Order;\n\npub struct DiscountClearance;\n\nimpl DiscountClearance {\n    pub fn discount_cents(&self, order: &Order) -> i64 {\n        todo!(\"discount_clearance contract: implement ({} items)\", order.items.len())\n    }\n}\n";

fn plan(contracts: serde_json::Value) -> String {
    serde_json::json!({ "contracts": contracts }).to_string()
}

fn contract(id: &str, instruction: &str, acceptance: &str, writes: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "instruction": instruction,
        "acceptance": acceptance,
        "writes": writes.iter().map(|w| serde_json::json!({"path": w})).collect::<Vec<_>>(),
        "tools": ["write_file"],
    })
}

async fn trace_events(dir: &Path) -> Vec<TraceEvent> {
    let path = dir.join("runs").join("r1").join("trace.jsonl");
    let started = Instant::now();
    loop {
        match TraceLog::open(&path).await {
            Ok(log) => break log.load().await.expect("load"),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("trace: {e:?}"),
        }
    }
}

fn refusals(events: &[TraceEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::PlanRefused { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

/// The traced plan repairs, as (ask, contract, change).
fn repairs(events: &[TraceEvent]) -> Vec<(u32, String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::PlanRepaired {
                ask,
                contract,
                change,
                ..
            } => Some((*ask, contract.clone(), change.clone())),
            _ => None,
        })
        .collect()
}

/// The first accepted plan's contracts and notices.
fn accepted(events: &[TraceEvent]) -> (Vec<kgraf_orchestrator::PlannedContract>, Vec<String>) {
    events
        .iter()
        .find_map(|e| match e {
            TraceEvent::PlanAccepted {
                contracts, notices, ..
            } => Some((contracts.clone(), notices.clone())),
            _ => None,
        })
        .expect("a plan was accepted")
}

/// Runs `start` with `instruction` over a repository holding `files`, with the
/// planner answering `first` and, once refused, `second`. Returns the planner's
/// prompts and the trace.
async fn planned(
    files: &[(&str, &str)],
    instruction: &str,
    first: String,
    second: String,
) -> (Vec<String>, Vec<TraceEvent>) {
    let dir = tempfile::tempdir().expect("dir");
    for (rel, text) in files {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.contains("<plan_refused>") {
                second.clone()
            } else {
                first.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    kgraf_graph::Indexer::new(&p.graph.lock().expect("graph"))
        .index()
        .expect("index");
    let mut services = p.services(WINDOW);
    services.workspaces = Arc::new(Rooted {
        dirs: DirWorkspaces {
            base: p.dir.join("worktrees"),
        },
        root: dir.path().to_path_buf(),
    });
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let h = o.handle();
    let text = instruction.to_owned();
    let run = tokio::spawn(async move { o.start(RunId::new("r1"), text).await });
    assert!(h.status().await.is_some(), "a plan was accepted");
    h.stop().await;
    run.await.expect("join").expect("planned");
    (p.planner.prompts(), trace_events(dir.path()).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_that_leaves_a_named_stub_unwritten_is_refused_then_accepted_with_it() {
    let leaves_it = plan(serde_json::json!([contract(
        "c4",
        "add CLR-1 to the clearance list in catalog/src/lib.rs",
        "cargo test -p catalog",
        &["catalog/src/lib.rs"]
    )]));
    let covers_it = plan(serde_json::json!([contract(
        "c4",
        "implement DiscountClearance::discount_cents and add CLR-1 to catalog/src/lib.rs",
        "cargo test -p catalog",
        &["catalog/src/lib.rs", "discount_clearance/src/lib.rs"]
    )]));
    let (prompts, refused) = planned(
        &[
            ("catalog/src/lib.rs", "pub const SKU_CLEARANCE_LIST: &[&str] = &[];\n"),
            ("discount_clearance/src/lib.rs", STUB),
        ],
        "Implement `DiscountClearance` in discount_clearance/src/lib.rs and edit catalog/src/lib.rs.",
        leaves_it,
        covers_it,
    )
    .await;
    // An unwritten scaffold has no mechanical remedy: asked again.
    assert!(repairs(&refused).is_empty());
    let refused = refusals(&refused);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains("`discount_clearance/src/lib.rs`"),
        "{}",
        refused[0]
    );
    assert!(
        refused[0].contains("no contract writes it"),
        "{}",
        refused[0]
    );
    assert_eq!(prompts.len(), 2, "one ask, one re-ask");
    assert!(prompts[1].contains("placeholder bodies"), "{}", prompts[1]);
}

/// Shaped like `crates/kgraf-host/src/scoped_check.rs` as of
/// 2026-10-02 (self-repo): the macro is named only in string literals, a raw
/// string and comments. No body is a placeholder.
const PROSE_ONLY: &str = r###"//! Scoped checks. `todo!()` in a module doc.
pub fn brief() -> &'static str {
    "Acceptance is advisory. `todo!()` panics or failing tests \
     in other contracts' unfinished code are not yours to fix."
}

pub fn raw() -> &'static str {
    r#"unimplemented!() in a "raw" string"#
}

// todo!() in a comment
pub fn f() {}
"###;

/// The self-repo run named such a file as context, the plan left it to no
/// contract, and all three asks were refused for an unwritten placeholder.
/// Text about a placeholder is not one: the first plan is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_named_file_that_mentions_a_placeholder_only_in_text_is_not_refused() {
    let first = plan(serde_json::json!([contract(
        "c1",
        "add a budget test in core/src/lib.rs",
        "cargo test -p core",
        &["core/src/lib.rs"]
    )]));
    let (prompts, refused) = planned(
        &[
            ("core/src/lib.rs", "pub fn budget() -> u32 { 1 }\n"),
            ("host/src/scoped_check.rs", PROSE_ONLY),
        ],
        "Add a budget test in core/src/lib.rs. See host/src/scoped_check.rs for the brief.",
        first.clone(),
        first,
    )
    .await;
    let refused = refusals(&refused);
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(prompts.len(), 1, "accepted at the first ask");
}

/// Greenfield, the first refusal's shape: c1 lists both members and runs
/// `cargo test --workspace`, and `wcl` is written only by c2, which runs at
/// the same time. kGraf makes c1 depend on c2; c2 then runs at a root whose
/// manifest comes later, so c2 writes it too. Accepted at the first ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_acceptance_before_its_members_exist_is_repaired_without_an_ask() {
    let deadlock = plan(serde_json::json!([
        contract(
            "c1",
            "Create the Cargo workspace at the repository root with `Cargo.toml` specifying members `[\"wcl-core\", \"wcl\"]`, and the `wcl-core` crate.",
            "Run `cargo test --workspace` locally; it must pass.",
            &["Cargo.toml", "wcl-core/Cargo.toml", "wcl-core/src/lib.rs"]
        ),
        contract(
            "c2",
            "Create the `wcl` binary crate.",
            "cargo test -p wcl",
            &["wcl/Cargo.toml", "wcl/src/main.rs"]
        ),
    ]));
    let (prompts, events) = planned(
        &[("README.md", "a word counter\n")],
        "Build a word-count tool as a Cargo workspace with wcl-core and wcl.",
        deadlock.clone(),
        deadlock,
    )
    .await;
    assert!(refusals(&events).is_empty(), "{:?}", refusals(&events));
    assert_eq!(prompts.len(), 1, "accepted at the first ask");
    let fixed = repairs(&events);
    assert_eq!(fixed.len(), 2, "{fixed:?}");
    assert_eq!((fixed[0].0, fixed[0].1.as_str()), (1, "c1"), "{fixed:?}");
    assert!(fixed[0].2.contains("depends on `c2`"), "{fixed:?}");
    assert_eq!(fixed[1].1, "c2", "{fixed:?}");
    assert!(fixed[1].2.contains("`Cargo.toml`"), "{fixed:?}");
    let (contracts, notices) = accepted(&events);
    assert_eq!(contracts[0].depends_on, vec!["c2".to_owned()]);
    assert!(contracts[1].writes.contains(&"Cargo.toml".to_owned()));
    assert_eq!(
        notices
            .iter()
            .filter(|n| n.contains("kGraf repaired it"))
            .count(),
        2,
        "{notices:?}"
    );
}

/// A member no contract writes has no mechanical remedy: the plan left it
/// out, so the planner is asked again with the refusal, as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_member_no_contract_writes_is_refused_then_accepted_when_one_contract_writes_them()
 {
    let leaves_it = plan(serde_json::json!([contract(
        "c1",
        "Create the Cargo workspace at the repository root with `Cargo.toml` specifying members `[\"wcl-core\", \"wcl\"]`, and the `wcl-core` crate.",
        "Run `cargo test --workspace` locally; it must pass.",
        &["Cargo.toml", "wcl-core/Cargo.toml", "wcl-core/src/lib.rs"]
    )]));
    let one = plan(serde_json::json!([contract(
        "c1",
        "Create the Cargo workspace at the repository root with `Cargo.toml` specifying members `[\"wcl-core\", \"wcl\"]`, and both crates.",
        "Run `cargo test --workspace` locally; it must pass.",
        &[
            "Cargo.toml",
            "wcl-core/Cargo.toml",
            "wcl-core/src/lib.rs",
            "wcl/Cargo.toml",
            "wcl/src/main.rs"
        ]
    )]));
    let (prompts, events) = planned(
        &[("README.md", "a word counter\n")],
        "Build a word-count tool as a Cargo workspace with wcl-core and wcl.",
        leaves_it,
        one,
    )
    .await;
    assert!(repairs(&events).is_empty());
    let refused = refusals(&events);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("`wcl/Cargo.toml`"), "{}", refused[0]);
    assert!(
        refused[0].contains("written by no contract"),
        "{}",
        refused[0]
    );
    assert!(
        refused[0].contains("cargo test --workspace"),
        "{}",
        refused[0]
    );
    assert_eq!(prompts.len(), 2, "one ask, one re-ask");
}

/// A new workspace: the root contract runs `cargo test -p wcl-core`, and
/// cargo loads every member manifest for `-p` too, so it could never pass.
/// The edge to the binary contract would be a cycle, so kGraf makes the
/// remedy the refusal names: the root contract writes the member manifest
/// and a minimal source (`main.rs`, which the binary contract fills in after
/// it). Accepted at the first ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_package_form_before_its_members_exist_is_repaired_to_the_named_remedy() {
    let mut bin = contract(
        "c2-bin",
        "Create the `wcl` binary crate.",
        "cargo test --workspace && echo SUCCESS",
        &["wcl/Cargo.toml", "wcl/src/main.rs"],
    );
    bin["depends_on"] = serde_json::json!(["c1-core"]);
    let deadlock = plan(serde_json::json!([
        contract(
            "c1-core",
            "Create the workspace root and `wcl-core` library.\n1. Write `Cargo.toml` as:\n[workspace]\nmembers = [\"wcl-core\", \"wcl\"]\n",
            "cargo test -p wcl-core && echo SUCCESS",
            &["Cargo.toml", "wcl-core/Cargo.toml", "wcl-core/src/lib.rs"]
        ),
        bin,
    ]));
    let (prompts, events) = planned(
        &[("README.md", "a word counter\n")],
        "Build a word-count tool as a Cargo workspace with wcl-core and wcl.",
        deadlock.clone(),
        deadlock,
    )
    .await;
    assert!(refusals(&events).is_empty(), "{:?}", refusals(&events));
    assert_eq!(prompts.len(), 1, "accepted at the first ask");
    let fixed = repairs(&events);
    assert_eq!(fixed.len(), 2, "{fixed:?}");
    assert!(fixed.iter().all(|(ask, c, _)| *ask == 1 && c == "c1-core"));
    assert!(fixed[0].2.contains("`wcl/Cargo.toml`"), "{fixed:?}");
    assert!(fixed[1].2.contains("`wcl/src/main.rs`"), "{fixed:?}");
    let (contracts, _) = accepted(&events);
    assert!(contracts[0].depends_on.is_empty(), "no edge to c2-bin");
    for path in ["wcl/Cargo.toml", "wcl/src/main.rs"] {
        assert!(contracts[0].writes.contains(&path.to_owned()), "{path}");
    }
}
