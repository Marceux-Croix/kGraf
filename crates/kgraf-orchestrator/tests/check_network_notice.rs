//! A run whose checks and gates run with the network open (the sandbox
//! cannot deny it: Windows, decision 0011 amended 2026-10-06) says so in its
//! trace, once, and in its report. A run whose backend denies the network
//! says nothing. The host decides ([`kgraf_sandbox::CheckNetwork`]) and hands
//! the sentence over; this is the orchestrator's half.
//!
//! How to run: `cargo test -p kgraf-orchestrator --test check_network_notice`.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus, TraceEvent};
use serde_json::json;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

fn plan() -> String {
    json!({"contracts": [{
        "id": "c1",
        "instruction": "add a test WRITE a.txt",
        "acceptance": "a.txt exists",
        "writes": [{"path": "a.txt"}],
        "tools": ["write_file"],
        "attempts": 1,
    }]})
    .to_string()
}

async fn run(notice: Option<&str>) -> (kgraf_orchestrator::RunReport, Vec<TraceEvent>) {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut services = p.services(WINDOW);
    services.workspaces = FakeIntegration::new(dir.path().join("worktrees"));
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .with_check_network_notice(notice.map(str::to_owned))
        .start(RunId::new("r1"), "add a test".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let events = kgraf_orchestrator::TraceLog::open(&dir.path().join("runs/r1/trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    (report, events)
}

fn traced<'a>(events: &'a [TraceEvent], needle: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Notice { detail, .. } if detail.contains(needle) => Some(detail.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_network_for_checks_is_traced_once_and_in_the_report() {
    let sentence = kgraf_sandbox::NETWORK_OPEN_NOTICE;
    let (report, events) = run(Some(sentence)).await;
    assert_eq!(
        traced(&events, "network not blocked"),
        vec![sentence],
        "{events:?}"
    );
    assert!(
        report.notices.iter().any(|n| n == sentence),
        "{:?}",
        report.notices
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backend_that_denies_the_network_adds_no_notice() {
    let (report, events) = run(None).await;
    assert!(
        traced(&events, "network not blocked").is_empty(),
        "{events:?}"
    );
    assert!(
        !report
            .notices
            .iter()
            .any(|n| n.contains("network not blocked")),
        "{:?}",
        report.notices
    );
}

/// The contract-check runner: standing in for a backend that cannot deny the
/// network, a check runs and its result ends with the notice, a failure and
/// a pass alike; with the default for this platform (denied on macOS and
/// Linux) nothing is added.
#[cfg(unix)]
mod runner {
    use kgraf_orchestrator::{CheckBase, SandboxRunner};
    use kgraf_sandbox::{CheckNetwork, NETWORK_OPEN_NOTICE};

    fn runner(
        network: Option<CheckNetwork>,
    ) -> (tempfile::TempDir, tempfile::TempDir, SandboxRunner) {
        let root = tempfile::tempdir().expect("root");
        let scratch = tempfile::tempdir().expect("scratch");
        let mut r = SandboxRunner::new(
            CheckBase::Directory(root.path().to_path_buf()),
            scratch.path().to_path_buf(),
            vec![],
        )
        .expect("runner")
        .accept_partial_isolation(!cfg!(target_os = "macos"));
        if let Some(n) = network {
            r = r.with_check_network(n);
        }
        (root, scratch, r)
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_contract_check_with_the_network_open_says_so() {
        let (_root, _scratch, r) = runner(Some(CheckNetwork::open_with_notice()));
        let pass = r.run(&[], &sh("echo fine")).await.expect("ran");
        assert!(pass.success, "{pass:?}");
        assert!(pass.tail.ends_with(NETWORK_OPEN_NOTICE), "{}", pass.tail);
        assert!(
            pass.tail.contains("[sandbox] network not blocked"),
            "{}",
            pass.tail
        );
        let fail = r.run(&[], &sh("echo bad >&2; exit 3")).await.expect("ran");
        assert!(!fail.success, "{fail:?}");
        assert!(fail.tail.contains("bad"), "{}", fail.tail);
        assert!(fail.tail.ends_with(NETWORK_OPEN_NOTICE), "{}", fail.tail);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_contract_check_on_a_backend_that_denies_the_network_adds_nothing() {
        let (_root, _scratch, r) = runner(None);
        let pass = r.run(&[], &sh("echo fine")).await.expect("ran");
        assert!(pass.success, "{pass:?}");
        assert!(!pass.tail.contains("network not blocked"), "{}", pass.tail);
    }

    /// With the network open a contract check reaches a local socket; with
    /// it denied the same check does not (macOS denies it).
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_decision_changes_what_a_contract_check_can_reach() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let probe = sh(&format!("/bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}'"));
        let (_a, _b, open) = runner(Some(CheckNetwork::open_with_notice()));
        assert!(open.run(&[], &probe).await.expect("ran").success);
        let (_c, _d, denied) = runner(None);
        assert!(!denied.run(&[], &probe).await.expect("ran").success);
    }
}
