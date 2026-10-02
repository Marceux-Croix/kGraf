//! Plan intake against the repository: a plan that leaves a named scaffold
//! file unwritten, or whose acceptance builds a workspace before its members
//! exist, is refused with a reason the planner can act on and is asked again.
//!
//! Round evidence: conflicts-tier2 gave the clearance discount to a contract
//! that wrote only `catalog/src/lib.rs`, leaving the stub
//! `discount_clearance/src/lib.rs` to no contract, and its worker was held at
//! the file; greenfield wrote `Cargo.toml` with members `wcl-core` and `wcl` in
//! one contract and `wcl` in another, with `cargo test --workspace` accepting
//! the first. The planner, grounder and workers are fake providers.

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

async fn refusals(dir: &Path) -> Vec<String> {
    let path = dir.join("runs").join("r1").join("trace.jsonl");
    let started = Instant::now();
    let events = loop {
        match TraceLog::open(&path).await {
            Ok(log) => break log.load().await.expect("load"),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("trace: {e:?}"),
        }
    };
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::PlanRefused { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

/// Runs `start` with `instruction` over a repository holding `files`, with the
/// planner answering `first` and, once refused, `second`. Returns the planner's
/// prompts and the refusals traced.
async fn planned(
    files: &[(&str, &str)],
    instruction: &str,
    first: String,
    second: String,
) -> (Vec<String>, Vec<String>) {
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
    (p.planner.prompts(), refusals(dir.path()).await)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_acceptance_before_its_members_exist_is_refused_then_accepted_when_one_contract_writes_them()
 {
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
    let (prompts, refused) = planned(
        &[("README.md", "a word counter\n")],
        "Build a word-count tool as a Cargo workspace with wcl-core and wcl.",
        deadlock,
        one,
    )
    .await;
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("`wcl/Cargo.toml`"), "{}", refused[0]);
    assert!(refused[0].contains("`c1`"), "{}", refused[0]);
    assert!(
        refused[0].contains("cargo test --workspace"),
        "{}",
        refused[0]
    );
    assert_eq!(prompts.len(), 2, "one ask, one re-ask");
}
