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
        assert_eq!(prompts.len(), 2, "one cut answer, one retry: {prompts:?}");
        // The retry shows the model where its answer broke.
        assert!(prompts[1].contains("<answer_refused>"), "{}", prompts[1]);
        assert!(prompts[1].contains("[ERROR HERE]"), "{}", prompts[1]);
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
        assert_eq!(rows, 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_grounding_malformed_every_time_holds_after_the_bound_and_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(dir.path(), CUT_WITH_ERROR_TAIL, false);
    let report = run(&p).await;
    assert!(matches!(report.status, RunStatus::Blocked { .. }));
    assert_eq!(report.verified, vec!["a".to_owned()]);
    assert_eq!(
        p.grounder.calls(),
        1 + kgraf_provider::MAX_MALFORMED_OUTPUT_RETRIES as usize,
        "asked once and again up to the bound, never more"
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
