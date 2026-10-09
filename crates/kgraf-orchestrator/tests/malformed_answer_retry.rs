//! A JSON-asked orchestrator call whose answer is malformed is asked again
//! within the shared bound (`kgraf_provider::MAX_MALFORMED_OUTPUT_RETRIES`)
//! before it fails, here through re-grounding, the call that held a run on
//! its first cut-off answer.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{ContractStatus, Orchestrator, RunStatus, RunStore};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A recorded local-model answer: a normal stop, with the `reason` string
/// never closed and no closing brace.
const CUT_MID_STRING: &str = "{\"serves\":true,\"reason\":\"The next step directly implements \
requirement 6 of the original instruction, which specifies exactly how to construct and \
return the result in `checkout::checkout_order`.";

/// The same answer with an error marker where the text stops.
const CUT_WITH_ERROR_TAIL: &str = "{\"serves\":true,\"reason\":\"The next step directly \
implements requirement 6 of the original instruction, which specifies exactly how to \
construct and return the result in `checkout::checkout_order`[ERROR";

fn record_of(p: &Parts) -> kgraf_orchestrator::RunStateRecord {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
}

/// Grounds `b` with `first` until the prompt carries the refusal of an
/// earlier answer, then with a whole verdict.
fn parts(dir: &std::path::Path, first: &'static str, recover: bool) -> Parts {
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    Parts::open(
        dir,
        move |_| plan.clone(),
        move |prompt| {
            if prompt.contains("Next step `b`") && !(recover && prompt.contains("<answer_refused>"))
            {
                first.to_owned()
            } else {
                SERVES.into()
            }
        },
        Duration::from_millis(5),
    )
}

async fn run(p: &Parts) -> kgraf_orchestrator::RunReport {
    let mut cfg = config();
    cfg.reground_every = 1;
    Orchestrator::new(cfg, p.services(WINDOW))
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a then b".into())
        .await
        .expect("run")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_grounding_answer_cut_mid_string_is_asked_again_and_the_step_runs() {
    for cut in [CUT_MID_STRING, CUT_WITH_ERROR_TAIL] {
        let dir = tempfile::tempdir().expect("dir");
        let p = parts(dir.path(), cut, true);
        let report = run(&p).await;
        assert_eq!(report.status, RunStatus::Completed, "{cut}");
        assert_eq!(report.verified, vec!["a".to_owned(), "b".to_owned()]);
        let prompts = p.grounder.prompts();
        // The cut answer cannot be closed (it stops inside a string): the
        // same request is sent once more, inside the same ask. The script
        // answers it cut again, which is the ask's refusal; the retry then
        // shows the model where its answer broke.
        assert_eq!(prompts.len(), 3, "cut, cut again, retry: {prompts:?}");
        assert!(
            prompts[1].ends_with(kgraf_orchestrator::INCOMPLETE_ANSWER_NOTE),
            "{}",
            prompts[1]
        );
        assert!(!prompts[1].contains("<answer_refused>"), "{}", prompts[1]);
        assert!(prompts[2].contains("<answer_refused>"), "{}", prompts[2]);
        assert!(prompts[2].contains("[ERROR HERE]"), "{}", prompts[2]);
        let record = record_of(&p);
        let g = &record.groundings[0];
        assert!(g.serves);
        assert!(g.call.is_some(), "the verdict names its ledger call");
        // Both calls are in the ledger, attributed to the run.
        let rows = p
            .ledger
            .calls()
            .expect("rows")
            .into_iter()
            .filter(|r| r.task_id == "r1" && r.trace.kind.as_deref() == Some("reground"))
            .count();
        assert_eq!(rows, 3);
        // The free second call is said in the trace and in the run's notices.
        let events = kgraf_orchestrator::TraceLog::open(&dir.path().join("runs/r1/trace.jsonl"))
            .await
            .expect("trace")
            .load()
            .await
            .expect("load");
        let reasked = events
            .iter()
            .filter(|e| {
                matches!(e, kgraf_orchestrator::TraceEvent::AnswerReasked { what, .. } if what == "reground")
            })
            .count();
        assert_eq!(reasked, 1, "{events:?}");
        assert!(
            report
                .notices
                .iter()
                .any(|n| n.contains("the same request was sent once more")),
            "{:?}",
            report.notices
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_grounding_malformed_every_time_holds_after_the_bound_and_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(dir.path(), CUT_WITH_ERROR_TAIL, false);
    let report = run(&p).await;
    assert!(matches!(report.status, RunStatus::Blocked { .. }));
    assert_eq!(report.verified, vec!["a".to_owned()]);
    // Each ask's answer stops inside a string, so each ask also sends its
    // request once more, outside the bound: two calls per ask.
    assert_eq!(
        p.grounder.calls(),
        2 * (1 + kgraf_provider::MAX_MALFORMED_OUTPUT_RETRIES as usize),
        "asked once and again up to the bound, each with one free call, never more"
    );
    let record = record_of(&p);
    let b = record.contract("b").expect("b");
    let ContractStatus::Held { reason } = &b.status else {
        panic!("b is not held: {:?}", b.status);
    };
    assert!(reason.contains("re-grounding failed"), "{reason}");
    assert!(
        reason.contains("the call was made 3 times and every answer was malformed"),
        "{reason}"
    );
    assert!(reason.contains("[ERROR HERE]"), "{reason}");
    // The failed verdict names the last call, so its answer can be found.
    assert!(record.groundings[0].call.is_some());
}

/// The server's answer to a retried orchestrator call is kept in the run's
/// trace, tied to the failed call's ledger id. The row settles unreconciled
/// with no error text, so without the record nothing in the run's files says
/// why (a worker transcript had the same gap). `status` and
/// `server_parse` are fields, so a reader never has to read the text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_call_after_a_server_error_leaves_its_status_in_the_trace() {
    for (status, body) in [
        (
            500,
            "XML syntax error on line 21: illegal character code U+0008",
        ),
        (200, "error parsing tool call: unexpected EOF"),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let plan = plan_json(&[
            c("a", "make a", "src/a.rs", &[]),
            c("b", "make b", "src/b.rs", &["a"]),
        ]);
        let reply = format!("SERVER_PARSE_ERROR:{status}:{body}");
        // A server error is asked again unchanged, so only the first ask fails.
        let failed_once = std::sync::atomic::AtomicBool::new(false);
        let p = Parts::open(
            dir.path(),
            move |_| plan.clone(),
            move |prompt| {
                if prompt.contains("Next step `b`")
                    && !failed_once.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    reply.clone()
                } else {
                    SERVES.into()
                }
            },
            Duration::from_millis(5),
        );
        let report = run(&p).await;
        assert_eq!(report.status, RunStatus::Completed, "{report:?}");

        let failed: Vec<String> = p
            .ledger
            .calls()
            .expect("rows")
            .into_iter()
            .filter(|r| {
                r.trace.kind.as_deref() == Some("reground")
                    && r.settlement == kgraf_provider::Settlement::Unreconciled
            })
            .map(|r| r.id.to_string())
            .collect();
        assert_eq!(failed.len(), 1, "one failed call: {failed:?}");

        let events = kgraf_orchestrator::TraceLog::open(&dir.path().join("runs/r1/trace.jsonl"))
            .await
            .expect("trace")
            .load()
            .await
            .expect("load");
        let kept: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                kgraf_orchestrator::TraceEvent::CallRetried {
                    call,
                    agent,
                    what,
                    status,
                    server_parse,
                    error,
                    ..
                } => Some((call, agent, what, *status, *server_parse, error)),
                _ => None,
            })
            .collect();
        assert_eq!(kept.len(), 1, "{events:?}");
        let (call, agent, what, got_status, server_parse, error) = kept[0];
        assert_eq!(call, &failed[0], "tied to the failed call's id");
        assert_eq!(agent, "orchestrator:r1");
        assert_eq!(what, "reground");
        assert_eq!(got_status, Some(status));
        assert!(server_parse);
        assert!(error.contains(&format!("HTTP {status}")), "{error}");
        assert!(error.contains(body), "{error}");
    }
}

/// A refused answer that quotes "HTTP 500" is the model's text, not a server
/// fault: no status, no parse flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_refused_answer_is_not_recorded_as_a_server_fault() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        "{\"serves\":true,\"reason\":\"the log said HTTP 500 and then",
        true,
    );
    let report = run(&p).await;
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let events = kgraf_orchestrator::TraceLog::open(&dir.path().join("runs/r1/trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let kept: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            kgraf_orchestrator::TraceEvent::CallRetried {
                status,
                server_parse,
                ..
            } => Some((*status, *server_parse)),
            _ => None,
        })
        .collect();
    assert_eq!(kept, vec![(None, false)], "{events:?}");
}

async fn trace_of(dir: &std::path::Path) -> Vec<kgraf_orchestrator::TraceEvent> {
    kgraf_orchestrator::TraceLog::open(&dir.join("runs/r1/trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

/// A contracts answer the model ended with only its final brace missing is
/// closed on the same call: no refusal is recorded, no ask is spent, and the
/// trace and the run's notices say what was appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_missing_its_final_brace_is_closed_and_no_ask_is_spent() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let cut = plan
        .trim_end()
        .strip_suffix('}')
        .expect("ends with }")
        .to_owned();
    let p = Parts::open(
        dir.path(),
        move |_| cut.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = run(&p).await;
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(p.planner.calls(), 1, "closed on the same call");
    let events = trace_of(dir.path()).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, kgraf_orchestrator::TraceEvent::PlanRefused { .. })),
        "{events:?}"
    );
    let repaired: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            kgraf_orchestrator::TraceEvent::AnswerRepaired { what, appended, .. } => {
                Some((what.as_str(), appended.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(repaired, vec![("decompose", "}")], "{events:?}");
    assert!(
        report.notices.iter().any(|n| n.contains("appended `}`")),
        "{:?}",
        report.notices
    );
}

/// A contracts answer that stops inside a string is sent once more, and that
/// second call is not a refused ask: no `plan_refused` is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_stopped_mid_string_is_asked_once_more_without_a_refusal() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let cut = plan[..plan.find("make b").expect("b") + 3].to_owned();
    let p = Parts::open(
        dir.path(),
        move |prompt| {
            if prompt.contains(kgraf_orchestrator::INCOMPLETE_ANSWER_NOTE) {
                plan.clone()
            } else {
                cut.clone()
            }
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let report = run(&p).await;
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(p.planner.calls(), 2);
    let events = trace_of(dir.path()).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, kgraf_orchestrator::TraceEvent::PlanRefused { .. })),
        "{events:?}"
    );
    let reasked = events
        .iter()
        .filter(|e| {
            matches!(e, kgraf_orchestrator::TraceEvent::AnswerReasked { what, .. } if what == "decompose")
        })
        .count();
    assert_eq!(reasked, 1, "{events:?}");
}
