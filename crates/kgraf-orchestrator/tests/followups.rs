//! Orchestrator follow-ups after merge-back: numbered verifications,
//! per-attempt worker environments, reopening a worker's recorded worktree,
//! immediate takeover of a dead owner's run lease, and conversation files
//! written outside the user's checkout. Real git for each is tested in
//! `kgraf-sandbox` (`tests/reopen.rs`) and `kgraf-host`
//! (`src/tests.rs`); here the orchestrator's side runs with fake models.

mod support;

use kgraf_core::{AgentId, RunId, TaskId};
use kgraf_graph::{Acquisition, LeaseRegistry};
use kgraf_orchestrator::{
    AttemptEnv, AttemptEnvs, COMMS_DIR, CloseEnv, DirWorkspaces, GateRequest, GateRunner,
    GateStatus, GateVerdict, Integration, OpenContract, Orchestrator, OrchestratorError, ROOT_GATE,
    Route, RunStateRecord, RunStatus, RunStore, TraceEvent, TraceLog, UntrustedText, Workspaces,
};
use kgraf_sandbox::{HostId, ProcessIdentity};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

/// Plain directories that record what the orchestrator hands them: the
/// recorded handle `prepare` saw per worker, every release, and a conversation
/// worktree outside the graph's root.
struct Recording {
    dirs: DirWorkspaces,
    comms: Option<PathBuf>,
    seen: Mutex<Vec<(String, Option<String>)>>,
    released: Mutex<Vec<(PathBuf, bool)>>,
    committed: Mutex<Vec<(PathBuf, Vec<String>)>>,
}

impl Recording {
    fn new(dir: &Path, comms: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            dirs: DirWorkspaces {
                base: dir.join("worktrees"),
            },
            comms,
            seen: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
            committed: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl Workspaces for Recording {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &AgentId,
    ) -> Result<PathBuf, kgraf_orchestrator::PrepareFailure> {
        self.seen.lock().expect("lock").push((
            worker.to_string(),
            contract.worktrees.get(worker.as_str()).cloned(),
        ));
        self.dirs.prepare(contract, worker).await
    }

    async fn handle(&self, workspace: &Path) -> Option<String> {
        Some(format!("recorded:{}", workspace.display()))
    }

    async fn release(&self, workspace: &Path, keep: bool) {
        self.released
            .lock()
            .expect("lock")
            .push((workspace.to_path_buf(), keep));
    }

    async fn open_comms(&self) -> Result<Option<PathBuf>, String> {
        Ok(self.comms.clone())
    }

    async fn commit_comms(
        &self,
        root: &Path,
        files: &[String],
        _message: &str,
    ) -> Result<Integration, String> {
        self.committed
            .lock()
            .expect("lock")
            .push((root.to_path_buf(), files.to_vec()));
        Ok(Integration::Merged {
            commit: format!("{:040x}", 7),
        })
    }
}

/// A pass voided by a lost lease
/// is verified again: the second verification gets the next number (so its
/// attempt ref cannot collide with the first's), the worker's worktree is kept
/// rather than discarded, and the re-dispatch hands `prepare` the handle
/// recorded for that worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_voided_pass_is_verified_again_under_the_next_number_in_its_kept_worktree() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    *p.verifier.delay.lock().expect("lock") = Duration::from_millis(400);
    let ws = Recording::new(dir.path(), None);
    let mut services = p.services(WINDOW);
    services.workspaces = ws.clone();
    let mut cfg = config();
    cfg.lease_ttl_ms = 60_000;
    let o = Orchestrator::new(cfg, services).expect("orchestrator");
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    let verifier = p.verifier.clone();
    until("verification has begun", || {
        verifier.started.load(Ordering::SeqCst) >= 1
    })
    .await;
    {
        let g = p.graph.lock().expect("lock");
        let reg = LeaseRegistry::new(&g).with_ttl_ms(300);
        let holder = reg
            .holder_of("src/a.rs", now_ms())
            .expect("registry")
            .expect("held")
            .agent_id;
        reg.release_all(&holder).expect("release");
        let got = reg
            .acquire_all("thief", "t", &["src/a.rs".to_owned()], &[], now_ms())
            .expect("acquire");
        assert!(matches!(got, Acquisition::Granted(_)));
    }
    let report = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("in time")
        .expect("join")
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(
        *p.verifier.numbers.lock().expect("lock"),
        vec![1, 2],
        "each verification numbered from the persisted counter"
    );
    let worktree = dir.path().join("worktrees").join("r1-a-r0-a0");
    let released = ws.released.lock().expect("lock").clone();
    assert_eq!(
        released.first(),
        Some(&(worktree.clone(), true)),
        "the voided attempt's worktree is kept for its worker: {released:?}"
    );
    let seen = ws.seen.lock().expect("lock").clone();
    let expected = format!("recorded:{}", worktree.display());
    assert_eq!(
        seen,
        vec![
            ("r1-a-r0-a0".to_owned(), None),
            ("r1-a-r0-a0".to_owned(), Some(expected)),
        ],
        "the re-dispatch reopens by the recorded handle"
    );
}

/// Every attempt runs in the environment the
/// factory built for its own worktree, which is closed after the worker ends;
/// a factory failure fails the attempt loudly, never falling back to the
/// route's environment.
struct Factory {
    built: Mutex<Vec<(String, PathBuf)>>,
    closed: Arc<AtomicUsize>,
    /// Builds still to fail.
    fail: AtomicUsize,
}

struct Closer(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl CloseEnv for Closer {
    async fn close(self: Box<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl AttemptEnvs for Factory {
    async fn build(
        &self,
        route: &Route,
        workspace: &Path,
        worker: &AgentId,
        _task: &TaskId,
    ) -> Result<AttemptEnv, String> {
        if self
            .fail
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err("MCP server `x` did not answer".into());
        }
        self.built
            .lock()
            .expect("lock")
            .push((worker.to_string(), workspace.to_path_buf()));
        Ok(AttemptEnv {
            env: route.env.clone(),
            closer: Some(Box::new(Closer(self.closed.clone()))),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_attempt_runs_in_the_environment_built_for_its_worktree() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let factory = Arc::new(Factory {
        built: Mutex::new(Vec::new()),
        closed: Arc::new(AtomicUsize::new(0)),
        fail: AtomicUsize::new(0),
    });
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_attempt_envs(factory.clone())
        .start(RunId::new("r1"), "make a and b".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let mut built = factory.built.lock().expect("lock").clone();
    built.sort();
    let base = dir.path().join("worktrees");
    assert_eq!(
        built,
        vec![
            ("r1-a-r0-a0".to_owned(), base.join("r1-a-r0-a0")),
            ("r1-b-r0-a0".to_owned(), base.join("r1-b-r0-a0")),
        ]
    );
    assert_eq!(
        factory.closed.load(Ordering::SeqCst),
        2,
        "closed after each"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_whose_environment_cannot_be_built_fails_loudly() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let factory = Arc::new(Factory {
        built: Mutex::new(Vec::new()),
        closed: Arc::new(AtomicUsize::new(0)),
        fail: AtomicUsize::new(usize::MAX),
    });
    let report = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_attempt_envs(factory)
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert!(report.verified.is_empty(), "{report:?}");
    assert!(
        p.stats.requests().is_empty(),
        "no model round ran in a substitute environment"
    );
    // Infrastructure, not a model attempt. Retried with
    // backoff, then held, saying why; `failures` (max_retries) never moved.
    assert_eq!(report.counters.environment_failures, 4, "{report:?}");
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    let a = record.contract("a").expect("open");
    assert_eq!(a.failures, 0, "not counted against max_retries");
    assert!(
        matches!(&a.status, kgraf_orchestrator::ContractStatus::Held { reason }
            if reason.contains("environment could not be built 4 times")),
        "{:?}",
        a.status
    );
    let trace = trace_of(&p).await;
    assert!(
        trace.iter().any(|e| matches!(
            e,
            TraceEvent::WorkerFinished { outcome, .. } if outcome == "failed"
        )),
        "the failure is recorded"
    );
}

/// An environment that fails twice and then builds is
/// retried with backoff and the contract verifies; no model attempt was
/// charged against max_retries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_that_recovers_is_retried_and_the_contract_verifies() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let factory = Arc::new(Factory {
        built: Mutex::new(Vec::new()),
        closed: Arc::new(AtomicUsize::new(0)),
        fail: AtomicUsize::new(2),
    });
    let mut cfg = config();
    cfg.max_retries = 0;
    let report = Orchestrator::new(cfg, p.services(WINDOW))
        .expect("orchestrator")
        .with_attempt_envs(factory)
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["a".to_owned()]);
    assert_eq!(report.counters.environment_failures, 2);
}

/// An owner recorded on this machine and namespace before
/// its last boot is dead; the run is taken over at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_from_before_a_reboot_is_taken_over() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let me = ProcessIdentity::current();
    seed_run(
        &p,
        ProcessIdentity {
            // Even this very PID: in the previous boot it was someone else.
            pid: me.pid,
            start: me.start,
            host: Some(HostId {
                boot: Some("a-previous-boot".into()),
                ..HostId::current()
            }),
        },
        "5-5",
    );
    if HostId::current().machine.is_none() {
        // No machine id here (a slim Linux container): a reboot of this
        // machine cannot be told from another machine, so the run is kept.
        let err = resume(&p).await.expect_err("refused");
        assert!(matches!(err, OrchestratorError::RunBusy { .. }), "{err}");
        return;
    }
    let report = resume(&p).await.expect("taken over");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
}

/// A run whose owner process died is resumed at once: its run lease
/// (and the dead owner's contract leases) are taken over instead of waited
/// out for the 120 s TTL. The owner is named by PID and start time, so a PID
/// reused by another process does not keep the run; a live owner does.
fn seed_run(p: &Parts, owner: ProcessIdentity, nonce: &str) {
    let store = RunStore::new(p.graph.clone());
    store
        .save(&RunStateRecord::new(RunId::new("r1"), "make a", now_ms()))
        .expect("save");
    let holder = format!(
        "orchestrator:r1:pid={}:start={}:{}:{nonce}",
        owner.pid,
        owner.start.unwrap_or(0),
        owner.host.clone().unwrap_or_default().fields()
    );
    let g = p.graph.lock().expect("lock");
    let reg = LeaseRegistry::new(&g).with_ttl_ms(120_000);
    for (who, key) in [
        (holder.clone(), "run:r1".to_owned()),
        (format!("r1/a@{nonce}"), "src/a.rs".to_owned()),
    ] {
        let got = reg
            .acquire_all(&who, "r1", &[key], &[], now_ms())
            .expect("acquire");
        assert!(matches!(got, Acquisition::Granted(_)));
    }
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().expect("spawn");
    let pid = child.id();
    child.wait().expect("wait");
    pid
}

async fn resume(p: &Parts) -> Result<kgraf_orchestrator::RunReport, OrchestratorError> {
    tokio::time::timeout(
        Duration::from_secs(10),
        Orchestrator::new(config(), p.services(WINDOW))
            .expect("orchestrator")
            .resume(RunId::new("r1")),
    )
    .await
    .expect("not waited out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_owners_run_lease_is_taken_over_at_once() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    seed_run(
        &p,
        ProcessIdentity {
            pid: dead_pid(),
            start: None,
            host: Some(HostId::current()),
        },
        "1-1",
    );
    let report = resume(&p).await.expect("taken over");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(
        report.verified,
        vec!["a".to_owned()],
        "the dead owner's lease on src/a.rs went with it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reused_pid_does_not_keep_the_run() {
    let me = ProcessIdentity::current();
    let Some(start) = me.start else {
        return; // no start-time probe on this platform: nothing to distinguish
    };
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    // This process's PID, another start time: the owner was an earlier process.
    seed_run(
        &p,
        ProcessIdentity {
            pid: me.pid,
            start: Some(start + 1),
            host: me.host.clone(),
        },
        "2-2",
    );
    let report = resume(&p).await.expect("taken over");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_owner_keeps_the_run() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    seed_run(&p, ProcessIdentity::current(), "3-3");
    let err = resume(&p).await.expect_err("refused");
    assert!(matches!(err, OrchestratorError::RunBusy { .. }), "{err}");
}

/// `kill(pid, 0)` answers for this boot and PID
/// namespace only. An owner recorded under another (a container, or before a
/// reboot), or with no host recorded, is never taken over, even though its PID
/// is dead here. Another machine or namespace, or an
/// unreadable one, keeps the run; the same machine after a reboot does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_from_another_machine_or_namespace_is_never_taken_over() {
    let here = HostId::current();
    // Another machine runs another kernel, so its boot id differs too.
    let other_machine = HostId {
        machine: Some("another-machine".into()),
        boot: Some("another-machines-boot".into()),
        ..here.clone()
    };
    let other_ns = HostId {
        ns: Some("1".into()),
        ..here.clone()
    };
    for host in [other_machine, other_ns, HostId::default()] {
        let host = Some(host);
        let dir = tempfile::tempdir().expect("dir");
        let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
        let p = Parts::open(
            dir.path(),
            move |_| plan.clone(),
            |_| SERVES.into(),
            Duration::from_millis(5),
        );
        seed_run(
            &p,
            ProcessIdentity {
                pid: dead_pid(),
                start: None,
                host: host.clone(),
            },
            "4-4",
        );
        let err = resume(&p).await.expect_err("refused");
        assert!(
            matches!(err, OrchestratorError::RunBusy { .. }),
            "{host:?}: {err}"
        );
    }
}

/// Conversation files are written in the worktree the workspaces
/// provide (the integration branch's), indexed from there, and committed from
/// there; the graph's root, the user's checkout, gets nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conversation_files_are_written_and_indexed_outside_the_users_checkout() {
    let dir = tempfile::tempdir().expect("dir");
    let integration = tempfile::tempdir().expect("integration");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let ws = Recording::new(dir.path(), Some(integration.path().to_path_buf()));
    let mut services = p.services(WINDOW);
    services.workspaces = ws.clone();
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a, then b".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    let from_a = record
        .coordination
        .mailbox
        .messages
        .iter()
        .find(|m| m.from_contract == "a")
        .expect("a's report was posted");
    assert_eq!(from_a.files, vec![format!("{COMMS_DIR}/a--b.md")]);
    assert!(integration.path().join(&from_a.files[0]).is_file());
    assert!(
        !dir.path().join("docs").exists(),
        "nothing in the user's checkout"
    );
    assert_eq!(
        from_a.nodes.len(),
        1,
        "indexed from the integration worktree"
    );
    // After the merge the worktree is gone; a full walk
    // of the user's checkout (which lacks the file) keeps the message nodes the
    // mailbox references, and the bus's directory is the one the walker leaves.
    assert!(kgraf_graph::BUS_OWNED_DIRS.contains(&kgraf_orchestrator::COMMS_DIR));
    let written = std::fs::read_to_string(integration.path().join(&from_a.files[0])).expect("file");
    assert!(
        written.starts_with(kgraf_graph::BUS_FILE_TITLE),
        "the walker recognizes the bus's files by their title"
    );
    std::fs::remove_dir_all(integration.path().join("docs")).expect("retired");
    {
        let g = p.graph.lock().expect("lock");
        kgraf_graph::Indexer::new(&g).index().expect("full walk");
        let ids: Vec<String> = g
            .nodes_in_file(&from_a.files[0])
            .expect("nodes")
            .into_iter()
            .map(|n| n.id)
            .collect();
        for node in &from_a.nodes {
            assert!(ids.contains(node), "mailbox node {node} still resolves");
        }
    }
    let committed = ws.committed.lock().expect("lock").clone();
    assert!(
        committed
            .iter()
            .any(|(root, files)| root == integration.path() && files == &from_a.files),
        "{committed:?}"
    );
}

/// Workspaces whose `prepare` answers from a script first, then plain
/// directories.
struct Scripted {
    dirs: DirWorkspaces,
    script: Mutex<Vec<kgraf_orchestrator::PrepareFailure>>,
    /// Every commit `commit_landed` was asked about, in order: the ground-truth
    /// check `verify_completion_landed` runs for every contract the run counts
    /// as merged (a premerged one included, once it is recorded).
    landed: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Workspaces for Scripted {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &AgentId,
    ) -> Result<PathBuf, kgraf_orchestrator::PrepareFailure> {
        let next = {
            let mut s = self.script.lock().expect("lock");
            (!s.is_empty()).then(|| s.remove(0))
        };
        match next {
            Some(failure) => Err(failure),
            None => self.dirs.prepare(contract, worker).await,
        }
    }

    async fn release(&self, _workspace: &Path, _keep: bool) {}

    async fn commit_landed(&self, commit: &str) -> Result<bool, String> {
        self.landed.lock().expect("lock").push(commit.to_owned());
        Ok(true)
    }
}

/// A gate runner that always passes, recording each request: enough to prove
/// a gate actually ran rather than staying held.
struct AlwaysPassGate {
    runs: Mutex<Vec<GateRequest>>,
}

impl AlwaysPassGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runs: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl GateRunner for AlwaysPassGate {
    fn commands(&self) -> Vec<String> {
        vec!["cargo test --workspace".into()]
    }

    async fn run(&self, request: GateRequest) -> Result<GateVerdict, String> {
        self.runs.lock().expect("lock").push(request);
        Ok(GateVerdict {
            passed: true,
            by: "cargo test --workspace".into(),
            output: UntrustedText::new("ok"),
        })
    }
}

/// A worktree whose attempt was merged before the kill
/// (reopen retired it, `AlreadyMerged`) counts as verified: no model round,
/// no redo, no retry used, and the merge commit is the verified one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_merged_before_the_kill_counts_as_verified() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let merged = format!("{:040x}", 42);
    let mut services = p.services(WINDOW);
    let scripted = Arc::new(Scripted {
        dirs: DirWorkspaces {
            base: dir.path().join("worktrees"),
        },
        script: Mutex::new(vec![kgraf_orchestrator::PrepareFailure::AlreadyMerged {
            commit: merged.clone(),
        }]),
        landed: Mutex::new(Vec::new()),
    });
    services.workspaces = scripted.clone();
    let mut cfg = config();
    cfg.max_retries = 0;
    // A gate covering the run's only contract: without recording the
    // premerged commit as `Integration::Merged` for it, coord.rs's "not on
    // the integration branch" check holds this gate forever and the run
    // never reaches `Completed`.
    let gate = AlwaysPassGate::new();
    let report = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .with_gate_runner(gate.clone())
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["a".to_owned()]);
    assert!(
        p.stats
            .requests()
            .iter()
            .all(|r| !r.1.starts_with("make a")),
        "no worker round ran: nothing to redo"
    );
    assert_eq!(
        p.verifier.started.load(Ordering::SeqCst),
        0,
        "not verified again"
    );
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(
        record.last_verified_commit.map(|c| c.sha),
        Some(merged.clone()),
        "the merge that already happened is the verified commit"
    );
    assert_eq!(
        record.coordination.integration.get("a"),
        Some(&Integration::Merged {
            commit: merged.clone()
        }),
        "the premerged commit is recorded so a covering gate can see it landed"
    );
    let root_gate = record
        .coordination
        .gates
        .iter()
        .find(|g| g.id == ROOT_GATE)
        .expect("root gate");
    assert!(
        matches!(root_gate.status, GateStatus::Passed { .. }),
        "the covering gate ran and passed rather than holding forever: {:?}",
        root_gate.status
    );
    assert!(
        !gate.runs.lock().expect("lock").is_empty(),
        "the gate actually ran"
    );
    assert!(
        scripted.landed.lock().expect("lock").contains(&merged),
        "the run's own end checked the premerged commit against ground truth"
    );
}

/// A worktree that cannot be provided (a refused
/// reopen, an unreadable tree) is an environment failure: counted there,
/// retried with backoff, never charged to max_retries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_that_cannot_be_provided_is_an_environment_failure() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut services = p.services(WINDOW);
    services.workspaces = Arc::new(Scripted {
        dirs: DirWorkspaces {
            base: dir.path().join("worktrees"),
        },
        script: Mutex::new(vec![
            "a process started in this worktree is still running"
                .to_owned()
                .into(),
            "worktree unreadable".to_owned().into(),
        ]),
        landed: Mutex::new(Vec::new()),
    });
    let mut cfg = config();
    cfg.max_retries = 0;
    let report = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.counters.environment_failures, 2);
    assert_eq!(report.verified, vec!["a".to_owned()]);
}

/// Startup cleanup keeps every worktree a record names,
/// including a replaced worker's recorded worktree and a worktree a child
/// contract inherited from its handed-back parent (named by path only).
#[test]
fn referenced_worktrees_include_replaced_workers_and_inherited_worktrees() {
    let dir = tempfile::tempdir().expect("dir");
    let graph = Arc::new(Mutex::new(
        kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph"),
    ));
    let store = RunStore::new(graph);
    let mut record = RunStateRecord::new(RunId::new("r1"), "x", 0);
    let mut c = OpenContract::planned("a.h1", "r1/a.h1".into(), "rest", "done");
    c.workers = vec!["r1-a.h1-r1-a0".into()];
    c.worktrees.insert("r1-a.h1-r0-a0".into(), "gitlink".into());
    c.workspace = Some(dir.path().join("worktrees").join("r1-a-r0-a0"));
    record.open_contracts.push(c);
    store.save(&record).expect("save");
    let names = store.referenced_worktrees().expect("names");
    for n in ["r1-a.h1-r1-a0", "r1-a.h1-r0-a0", "r1-a-r0-a0"] {
        assert!(names.contains(n), "{n} in {names:?}");
    }
}

/// Workspaces that always refuse one worker's worktree, as a reopen refuses a
/// tampered or orphan-held one every time.
struct RefuseWorker {
    dirs: DirWorkspaces,
    refused: String,
    seen: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Workspaces for RefuseWorker {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &AgentId,
    ) -> Result<PathBuf, kgraf_orchestrator::PrepareFailure> {
        self.seen.lock().expect("lock").push(worker.to_string());
        if worker.as_str() == self.refused {
            return Err("reopen refused: the gitlink was altered".to_owned().into());
        }
        self.dirs.prepare(contract, worker).await
    }

    async fn release(&self, _workspace: &Path, _keep: bool) {}
}

/// A reopen refused every time used to repeat the same
/// worker id until the contract was held. The retry is now a fresh worker in
/// a fresh worktree, which verifies; max_retries is not charged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permanently_refused_reopen_is_retried_with_a_fresh_worker_that_verifies() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let ws = Arc::new(RefuseWorker {
        dirs: DirWorkspaces {
            base: dir.path().join("worktrees"),
        },
        refused: "r1-a-r0-a0".into(),
        seen: Mutex::new(Vec::new()),
    });
    let mut services = p.services(WINDOW);
    services.workspaces = ws.clone();
    let mut cfg = config();
    cfg.max_retries = 0;
    let report = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["a".to_owned()]);
    assert_eq!(
        *ws.seen.lock().expect("lock"),
        vec!["r1-a-r0-a0".to_owned(), "r1-a-r0n1-a0".to_owned()],
        "the refused worktree is not asked for again"
    );
    assert_eq!(report.counters.environment_failures, 1);
}

/// A worktree in use (an orphan still runs in it) is not a
/// refusal that repeats. The same worker waits in the backoff and then
/// continues in its own worktree, so its unmerged work is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_worktree_is_waited_for_by_the_same_worker() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    struct BusyTwice {
        dirs: DirWorkspaces,
        left: Mutex<u32>,
        seen: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl Workspaces for BusyTwice {
        async fn prepare(
            &self,
            contract: &OpenContract,
            worker: &AgentId,
        ) -> Result<PathBuf, kgraf_orchestrator::PrepareFailure> {
            self.seen.lock().expect("lock").push(worker.to_string());
            let busy = {
                let mut left = self.left.lock().expect("lock");
                let b = *left > 0;
                *left = left.saturating_sub(1);
                b
            };
            if busy {
                return Err(kgraf_orchestrator::PrepareFailure::Busy(
                    "a process started in this worktree is still running".into(),
                ));
            }
            self.dirs.prepare(contract, worker).await
        }
        async fn release(&self, _workspace: &Path, _keep: bool) {}
    }
    let mut services = p.services(WINDOW);
    services.workspaces = Arc::new(BusyTwice {
        dirs: DirWorkspaces {
            base: dir.path().join("worktrees"),
        },
        left: Mutex::new(2),
        seen: seen.clone(),
    });
    let mut cfg = config();
    cfg.max_retries = 0;
    let report = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.counters.environment_failures, 2);
    assert_eq!(
        *seen.lock().expect("lock"),
        vec!["r1-a-r0-a0".to_owned(); 3],
        "the same worker each time: its worktree is not replaced"
    );
}

/// Directories whose release "fails to remove" one worktree, as a provider
/// reports through `take_problems`.
struct FailsToRemove {
    dirs: DirWorkspaces,
    problem: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Workspaces for FailsToRemove {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &AgentId,
    ) -> Result<PathBuf, kgraf_orchestrator::PrepareFailure> {
        self.dirs.prepare(contract, worker).await
    }

    async fn release(&self, workspace: &Path, _keep: bool) {
        self.problem.lock().expect("lock").push(format!(
            "worktree {} could not be removed: access refused",
            workspace.display()
        ));
    }

    fn take_problems(&self) -> Vec<String> {
        std::mem::take(&mut *self.problem.lock().expect("lock"))
    }
}

/// A worktree the provider could not remove after use reaches the run's
/// report as a warning naming the worktree, not only a line on stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_that_could_not_be_removed_is_a_warning_in_the_report() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut services = p.services(WINDOW);
    services.workspaces = Arc::new(FailsToRemove {
        dirs: DirWorkspaces {
            base: dir.path().join("worktrees"),
        },
        problem: Mutex::new(Vec::new()),
    });
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("r1-a-r0-a0") && w.contains("could not be removed")),
        "{:?}",
        report.warnings
    );
}
