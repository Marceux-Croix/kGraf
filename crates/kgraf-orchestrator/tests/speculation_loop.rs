//! The loop's side of speculative merge (decision 0074): a round takes a work-item slot like a gate
//! and is skipped, traced, when none is free; a round considers at most `speculate_max_contracts`
//! contracts; its cost reaches the run record and the trace. The merge itself is tested against
//! real git in `kgraf-host`; here the provider only records what the loop asked of it.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    DirWorkspaces, Limits, OpenContract, Orchestrator, OrchestratorConfig, PrepareFailure,
    RunStatus, RunStore, TraceEvent, TraceLog, Trial, Workspaces,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// Plain directories, with a branch whose every trial merges cleanly; it
/// records every worktree a round snapshotted.
struct Recording {
    dirs: DirWorkspaces,
    snapshotted: Mutex<Vec<PathBuf>>,
    /// The largest number of distinct worktrees one round snapshotted.
    widest: Mutex<usize>,
    this_round: Mutex<BTreeSet<PathBuf>>,
}

#[async_trait::async_trait]
impl Workspaces for Recording {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &kgraf_core::AgentId,
    ) -> Result<PathBuf, PrepareFailure> {
        self.dirs.prepare(contract, worker).await
    }
    async fn release(&self, workspace: &Path, keep: bool) {
        self.dirs.release(workspace, keep).await
    }
    async fn integration_tip(&self) -> Result<Option<String>, String> {
        // A new round starts with the tip.
        let mut round = self.this_round.lock().expect("lock");
        let mut widest = self.widest.lock().expect("lock");
        *widest = (*widest).max(round.len());
        round.clear();
        Ok(Some("tip".into()))
    }
    async fn snapshot(&self, workspace: &Path) -> Result<Option<String>, String> {
        self.snapshotted
            .lock()
            .expect("lock")
            .push(workspace.to_path_buf());
        self.this_round
            .lock()
            .expect("lock")
            .insert(workspace.to_path_buf());
        Ok(Some(workspace.display().to_string()))
    }
    async fn trial_merge(&self, commits: &[String]) -> Result<Option<Trial>, String> {
        Ok(Some(Trial::Clean {
            commit: commits.join("+"),
            tree: commits.join("+"),
        }))
    }
}

fn recording(dir: &Path) -> Arc<Recording> {
    Arc::new(Recording {
        dirs: DirWorkspaces {
            base: dir.join("worktrees"),
        },
        snapshotted: Mutex::new(Vec::new()),
        widest: Mutex::new(0),
        this_round: Mutex::new(BTreeSet::new()),
    })
}

fn config() -> OrchestratorConfig {
    OrchestratorConfig {
        tick: Duration::from_millis(10),
        step_budget: 6,
        speculate_every: Some(Duration::from_millis(10)),
        speculate_max_contracts: 2,
        ..OrchestratorConfig::default()
    }
}

async fn run(dir: &Path, cap: usize, contracts: &[C<'_>]) -> (Parts, Arc<Recording>) {
    let plan = plan_json(contracts);
    let mut p = Parts::open(
        dir,
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(300),
    );
    p.admission = admission(Limits::new(cap, 8).expect("limits"));
    let ws = recording(dir);
    let mut services = p.services(WINDOW);
    services.workspaces = ws.clone();
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "edits".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    (p, ws)
}

async fn speculation_events(p: &Parts) -> Vec<(String, String)> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
        .into_iter()
        .filter_map(|e| match e {
            TraceEvent::Speculation {
                outcome, detail, ..
            } => Some((outcome, detail)),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_every_slot_taken_a_round_is_skipped_and_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    // Two contracts fill both slots: no round can take one.
    let (p, ws) = run(
        dir.path(),
        2,
        &[
            c("a", "edit a", "a.txt", &[]),
            c("b", "edit b", "b.txt", &[]),
        ],
    )
    .await;
    let events = speculation_events(&p).await;
    assert!(
        events
            .iter()
            .any(|(o, d)| o == "skipped" && d.contains("slot")),
        "{events:?}"
    );
    assert!(
        ws.snapshotted.lock().expect("lock").is_empty(),
        "no round ran"
    );
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert!(record.coordination.speculation.skipped > 0);
    assert_eq!(record.coordination.speculation.rounds, 0);
    assert_eq!(p.admission.gate().in_flight(), (0, 0), "no slot leaked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_round_considers_at_most_the_cap_gives_its_slot_back_and_records_its_cost() {
    let dir = tempfile::tempdir().expect("dir");
    // Three in flight with a slot to spare; a round looks at two of them.
    let (p, ws) = run(
        dir.path(),
        4,
        &[
            c("a", "edit a", "a.txt", &[]),
            c("b", "edit b", "b.txt", &[]),
            c("x", "edit x", "x.txt", &[]),
        ],
    )
    .await;
    assert!(
        !ws.snapshotted.lock().expect("lock").is_empty(),
        "a round ran"
    );
    let widest = (*ws.widest.lock().expect("lock")).max(ws.this_round.lock().expect("lock").len());
    assert_eq!(widest, 2, "a round considered {widest} contracts");
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert!(record.coordination.speculation.rounds > 0);
    let events = speculation_events(&p).await;
    assert!(
        events
            .iter()
            .any(|(o, d)| o == "round" && d.contains("this run:")),
        "{events:?}"
    );
    // Three contracts and one round's slot fit the cap of four; a round that
    // kept its slot would leave the next one without.
    assert!(!events.iter().any(|(o, _)| o == "skipped"), "{events:?}");
    assert_eq!(p.admission.gate().in_flight(), (0, 0), "no slot leaked");
}
