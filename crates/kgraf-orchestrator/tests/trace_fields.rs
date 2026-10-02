//! Trace fields a test reads to tell what an attempt was given and how it
//! ended: the brief digest on `dispatch`, and the last stop reason and the
//! tool-call count on `worker_finished`. Each test runs a real contract
//! against the fake providers and compares the field with what the provider
//! saw, not with a constant.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{Orchestrator, RunStatus, TraceEvent, TraceLog};
use kgraf_provider::StopReason;
use std::time::Duration;
use support::*;

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    let log = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace");
    log.load().await.expect("load")
}

async fn run_one(dir: &std::path::Path) -> Parts {
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir,
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let o = Orchestrator::new(config(), p.services(200_000)).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_carries_the_digest_of_the_brief_the_worker_received() {
    let dir = tempfile::tempdir().expect("dir");
    let p = run_one(dir.path()).await;

    // What the provider received as the first text of the first user message.
    let seen: Vec<String> = p
        .stats
        .requests()
        .into_iter()
        .filter(|(_, _, round)| *round == 0)
        .map(|(_, instruction, _)| instruction)
        .collect();
    assert_eq!(seen.len(), 1, "one worker ran: {seen:?}");
    let expected = blake3::hash(seen[0].as_bytes()).to_hex().to_string();

    let trace = trace_of(&p).await;
    let (digest, per_worker, workers) = trace
        .iter()
        .find_map(|e| match e {
            TraceEvent::Dispatch {
                contract,
                brief_digest,
                brief_digests,
                workers,
                ..
            } if contract == "a" => Some((brief_digest.clone(), brief_digests.clone(), workers)),
            _ => None,
        })
        .expect("a dispatch of `a`");
    assert_eq!(digest, expected, "the digest is of the text the worker got");
    assert_eq!(per_worker.get(&workers[0]), Some(&expected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_finished_carries_the_last_stop_and_the_tool_call_count() {
    let dir = tempfile::tempdir().expect("dir");
    let p = run_one(dir.path()).await;
    let trace = trace_of(&p).await;
    let finished = trace
        .iter()
        .find_map(|e| match e {
            TraceEvent::WorkerFinished {
                contract,
                last_stop,
                tool_calls,
                ..
            } if contract == "a" => Some((*last_stop, *tool_calls)),
            _ => None,
        })
        .expect("a finished worker");
    // The fake writes one file in round 0 and answers in round 1.
    assert_eq!(finished, (Some(StopReason::EndTurn), 1));
}

/// A trace written before the fields existed still loads, with empty values,
/// and the new fields appear in the written line under their exact names.
#[test]
fn old_trace_lines_parse_and_new_lines_name_the_fields() {
    let old_dispatch = r#"{"event":"dispatch","at_ms":1,"contract":"a","route":"r","model":"m","workers":["w"],"projected_tokens":1,"target_tokens":2,"projection":{"source":"unmeasured_default"},"estimate_micros":3}"#;
    let parsed: TraceEvent = serde_json::from_str(old_dispatch).expect("old dispatch");
    match &parsed {
        TraceEvent::Dispatch {
            brief_digest,
            brief_digests,
            ..
        } => {
            assert!(brief_digest.is_empty());
            assert!(brief_digests.is_empty());
        }
        other => panic!("{other:?}"),
    }
    let json = serde_json::to_value(&parsed).expect("write");
    assert_eq!(json["brief_digest"], "");
    assert!(json["brief_digests"].is_object());

    let old_finished = r#"{"event":"worker_finished","at_ms":1,"contract":"a","worker":"w","route":"r","model":"m","outcome":"completed","steps":2,"peak_context_tokens":1,"projected_tokens":1,"stop_tokens":1,"sizing_miss":false,"calls":[],"spend_micros":0}"#;
    let parsed: TraceEvent = serde_json::from_str(old_finished).expect("old finished");
    match &parsed {
        TraceEvent::WorkerFinished {
            last_stop,
            tool_calls,
            ..
        } => {
            assert_eq!(*last_stop, None);
            assert_eq!(*tool_calls, 0);
        }
        other => panic!("{other:?}"),
    }
    let json = serde_json::to_value(TraceEvent::WorkerFinished {
        at_ms: 1,
        contract: "a".into(),
        worker: "w".into(),
        route: "r".into(),
        model: "m".into(),
        outcome: "completed".into(),
        steps: 2,
        peak_context_tokens: 1,
        projected_tokens: 1,
        stop_tokens: 1,
        window_tokens: 0,
        sizing_miss: false,
        calls: vec![],
        spend_micros: 0,
        error: None,
        last_stop: Some(StopReason::MaxTokens),
        tool_calls: 4,
    })
    .expect("write");
    assert_eq!(json["last_stop"], "max_tokens");
    assert_eq!(json["tool_calls"], 4);
}

/// What the trace says was accepted equals what the workers received, and the
/// base each attempt started from is the integration branch tip at dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_accepted_equals_what_workers_received_and_dispatch_names_its_base() {
    let dir = tempfile::tempdir().expect("dir");
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
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    let base = integration.start_from_a_base();
    let mut services = p.services(200_000);
    services.workspaces = integration.clone();
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "make a and b".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let trace = trace_of(&p).await;

    let accepted: Vec<_> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::PlanAccepted {
                revision,
                contracts,
                interfaces,
                notices,
                ..
            } => Some((
                *revision,
                contracts.clone(),
                interfaces.clone(),
                notices.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(accepted.len(), 1, "{accepted:?}");
    let (revision, contracts, interfaces, notices) = &accepted[0];
    assert_eq!(*revision, 1);
    assert!(interfaces.is_empty() && notices.is_empty(), "{accepted:?}");
    let a = contracts.iter().find(|c| c.id == "a").expect("a");
    let b = contracts.iter().find(|c| c.id == "b").expect("b");
    assert_eq!(a.instruction, "make a WRITE src/a.rs");
    assert_eq!(a.acceptance, "src/a.rs exists");
    assert_eq!(a.writes, ["src/a.rs"]);
    assert!(a.depends_on.is_empty());
    assert_eq!(b.depends_on, ["a"]);

    // Each contract's instruction is the text its worker opened with.
    let seen: Vec<String> = p
        .stats
        .requests()
        .into_iter()
        .filter(|(_, _, round)| *round == 0)
        .map(|(_, instruction, _)| instruction)
        .collect();
    assert_eq!(seen.len(), 2, "{seen:?}");
    for contract in contracts {
        assert_eq!(
            seen.iter()
                .filter(|s| s.contains(&contract.instruction))
                .count(),
            1,
            "{}: {seen:?}",
            contract.id
        );
    }

    // Every dispatch names its base: the first the starting commit, the
    // second the commit the first merged.
    let mut bases = std::collections::BTreeMap::new();
    for e in &trace {
        if let TraceEvent::Dispatch {
            contract,
            base_commit,
            base_commits,
            workers,
            brief_digest,
            ..
        } = e
        {
            for w in workers {
                assert_eq!(base_commits.get(w), base_commit.as_ref());
            }
            assert!(!brief_digest.is_empty());
            bases.insert(contract.clone(), base_commit.clone());
        }
    }
    assert_eq!(bases["a"].as_deref(), Some(base.as_str()));
    let record = kgraf_orchestrator::RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("the run has a record");
    let merged_a = match record.coordination.integration.get("a") {
        Some(kgraf_orchestrator::Integration::Merged { commit }) => commit.clone(),
        other => panic!("a was not merged: {other:?}"),
    };
    let merged_b = match record.coordination.integration.get("b") {
        Some(kgraf_orchestrator::Integration::Merged { commit }) => commit.clone(),
        other => panic!("b was not merged: {other:?}"),
    };
    // The fake's commits are fixed-width hex counters, so they order as
    // strings. The branch also takes a coordination commit after a merge, so
    // b's base is a's merge or a later commit, and before b's own merge.
    let base_b = bases["b"].clone().expect("b has a base");
    assert!(
        merged_a <= base_b && base_b < merged_b,
        "{merged_a} <= {base_b} < {merged_b}"
    );
}

/// An old trace has none of the new fields; a new line names them.
#[test]
fn old_dispatch_lines_have_no_base_and_plan_accepted_fields_default() {
    let old_dispatch = r#"{"event":"dispatch","at_ms":1,"contract":"a","route":"r","model":"m","workers":["w"],"projected_tokens":1,"target_tokens":2,"projection":{"source":"unmeasured_default"},"estimate_micros":3}"#;
    match serde_json::from_str::<TraceEvent>(old_dispatch).expect("old dispatch") {
        TraceEvent::Dispatch {
            base_commit,
            base_commits,
            ..
        } => {
            assert_eq!(base_commit, None);
            assert!(base_commits.is_empty());
        }
        other => panic!("{other:?}"),
    }
    let line = r#"{"event":"plan_accepted","at_ms":1,"revision":2,"contracts":[{"id":"a","instruction":"i","acceptance":"x","writes":["p"],"depends_on":["z"],"route":"r"}]}"#;
    match serde_json::from_str::<TraceEvent>(line).expect("plan_accepted") {
        TraceEvent::PlanAccepted {
            revision,
            contracts,
            interfaces,
            notices,
            ..
        } => {
            assert_eq!(revision, 2);
            assert_eq!(contracts[0].route.as_deref(), Some("r"));
            assert_eq!(contracts[0].task_kind, None);
            assert!(interfaces.is_empty() && notices.is_empty());
        }
        other => panic!("{other:?}"),
    }
}
