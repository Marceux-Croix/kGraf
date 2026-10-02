//! Public shapes the repository already defines, through a running
//! orchestrator with fake planner and worker models:
//!
//! - a contract instruction that gives a type fields where the repository
//!   defines it as a unit struct is noticed at plan intake;
//! - a verifier refusal whose compile error lies in a file the contract does
//!   not write amends the retry once, instead of sending the same instruction
//!   again.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    DirWorkspaces, OpenContract, Orchestrator, PrepareFailure, TraceEvent, TraceLog, Verdict,
    VerifiedCommit, Verifier, Workspaces,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A compile error in a file another contract owns, as cargo prints it.
const OUTSIDE_ERROR: &str = "   Compiling pricing_engine v0.1.0\n\
error[E0423]: expected value, found struct `FlatDiscount`\n\
 --> pricing_engine/src/lib.rs:5:18\n\
  |\n\
5 |     let _ = (FlatDiscount, BulkDiscount);\n";

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

fn notices_in(trace: &[TraceEvent], needle: &str) -> usize {
    trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Notice { detail, .. } if detail.contains(needle)))
        .count()
}

// ---------------------------------------------------------------- unit structs

/// Plain directories that also name the repository root, as a git-backed
/// provider does.
struct WithRoot {
    dirs: DirWorkspaces,
    root: PathBuf,
}

#[async_trait::async_trait]
impl Workspaces for WithRoot {
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

    fn repo_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }
}

async fn plan_notices(instruction: &str) -> Vec<TraceEvent> {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(
        root.join("src/discount.rs"),
        "pub struct FlatDiscount;\npub struct Bulk { pub n: u32 }\n",
    )
    .expect("write");
    let plan = plan_json(&[c("c1", instruction, "src/discount.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let mut services = p.services(WINDOW);
    services.workspaces = Arc::new(WithRoot {
        dirs: DirWorkspaces {
            base: p.dir.join("worktrees"),
        },
        root,
    });
    Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "price orders".into())
        .await
        .expect("run");
    trace_of(&p).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instruction_that_gives_a_unit_struct_fields_is_noticed_not_refused() {
    let trace = plan_notices("Define `FlatDiscount { order: Order }` with `new(order)`").await;
    assert_eq!(
        notices_in(
            &trace,
            "instructs `FlatDiscount` with fields, but `src/discount.rs` defines it as a unit struct"
        ),
        1,
        "{trace:?}"
    );
    // The plan was taken: the contract ran.
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEvent::Dispatch { .. })),
        "a notice does not block: {trace:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instruction_that_keeps_the_shape_is_not_noticed() {
    // `Bulk` already has a field, and `FlatDiscount` is named without one.
    let trace =
        plan_notices("Implement `FlatDiscount` and change `Bulk { n: u32 }` freely, keeping `FlatDiscount;` as is")
            .await;
    assert_eq!(
        notices_in(&trace, "defines it as a unit struct"),
        0,
        "{trace:?}"
    );
}

// ----------------------------------------------------------------- retry amend

/// Fails the first `fail_first` verifications with `OUTSIDE_ERROR`, then
/// delegates to the file verifier.
struct OutsideErrors {
    inner: Arc<FileVerifier>,
    fail_first: usize,
    seen: AtomicUsize,
    /// The reason is kept for the test to read back.
    reasons: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Verifier for OutsideErrors {
    async fn verify(
        &self,
        contract: &OpenContract,
        workspace: &Path,
        result: &kgraf_runtime::WorkerResult,
    ) -> Result<Verdict, String> {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_first {
            self.reasons
                .lock()
                .expect("lock")
                .push(OUTSIDE_ERROR.to_owned());
            return Ok(Verdict::Fail {
                reason: OUTSIDE_ERROR.to_owned(),
            });
        }
        self.inner.verify(contract, workspace, result).await
    }

    async fn recheck(&self, commit: &VerifiedCommit) -> Result<bool, String> {
        self.inner.recheck(commit).await
    }
}

const AMENDMENT: &str = "The error is in `pricing_engine/src/lib.rs`, which uses `FlatDiscount` \
as the existing code defines it; keep the public shape that existing callers use.";

async fn run_with_outside_errors(
    fail_first: usize,
    max_retries: u32,
) -> (Vec<String>, Vec<TraceEvent>) {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c(
        "c1",
        "implement the discount",
        "flat_discount/src/lib.rs",
        &[],
    )]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let mut services = p.services(WINDOW);
    services.verifier = Arc::new(OutsideErrors {
        inner: p.verifier.clone(),
        fail_first,
        seen: AtomicUsize::new(0),
        reasons: Mutex::new(Vec::new()),
    });
    let mut cfg = config();
    cfg.max_retries = max_retries;
    let _ = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "implement the discount".into())
        .await
        .expect("run");
    let openings = p.stats.openings.lock().expect("lock").clone();
    (openings, trace_of(&p).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compile_error_outside_the_contracts_files_amends_the_retry() {
    let (openings, trace) = run_with_outside_errors(1, 1).await;
    assert!(openings.len() >= 2, "a retry ran: {openings:?}");
    assert!(
        !openings[0].contains("keep the public shape"),
        "the first attempt has no amendment: {}",
        openings[0]
    );
    assert!(
        openings[1].contains(AMENDMENT),
        "the retry carries the amendment, quoting the error: {}",
        openings[1]
    );
    assert!(
        openings[1].contains("expected value, found struct `FlatDiscount`"),
        "{}",
        openings[1]
    );
    assert_eq!(
        notices_in(&trace, "the retry is amended once"),
        1,
        "{trace:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_contract_is_amended_once_however_often_it_fails_the_same_way() {
    let (openings, trace) = run_with_outside_errors(3, 3).await;
    assert!(openings.len() >= 3, "retries ran: {}", openings.len());
    assert_eq!(
        notices_in(&trace, "the retry is amended once"),
        1,
        "bounded once per contract: {trace:?}"
    );
    for later in &openings[1..3] {
        assert!(later.contains(AMENDMENT), "{later}");
    }
}
