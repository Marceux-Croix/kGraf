//! A mailbox message (a verified contract's report, `coord.rs`) to a recipient
//! whose attempt is running is injected at that attempt's next round boundary
//! through the runtime's injector (decision 0067), falls back to the next fresh
//! brief when the attempt ends first, and is traced `undelivered` and reported
//! to its sender when the recipient closes without either.
//!
//! Every wait is on an event (a parked call, a transcript line, a saved record),
//! never on a fixed delay: the fakes park a call until the test adds a permit.
//! The polls below have a long deadline only so a broken build fails with a
//! message instead of hanging.

mod support;

use kgraf_core::session::SessionRecords;
use kgraf_core::{AgentId, RunId};
use kgraf_graph::sessions::SharedSessions;
use kgraf_orchestrator::{
    CopyState, Envelope, Orchestrator, RunStatus, RunStore, TraceEvent, TraceLog, queued_row_id,
};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;
/// The running recipient's first attempt.
const C_ATTEMPT: &str = "r1-c-r0-a0";

fn try_record(p: &Parts) -> Option<kgraf_orchestrator::RunStateRecord> {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .ok()
        .flatten()
}

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

fn transcript_path(p: &Parts, worker: &str) -> PathBuf {
    p.dir
        .join("runs")
        .join("r1")
        .join("workers")
        .join(format!("{worker}.jsonl"))
}

fn transcript_lines(p: &Parts, worker: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(transcript_path(p, worker))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Poll `f` until it holds. The deadline is a failure message for a broken
/// build, not a synchronization point.
async fn wait_for(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !f() {
        assert!(Instant::now() < deadline, "never happened: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `a`'s message, once the record holds a copy of it queued for `worker`.
fn queued_copy(p: &Parts, worker: &str) -> Option<Envelope> {
    try_record(p)?
        .coordination
        .mailbox
        .messages
        .into_iter()
        .find(|m| m.from_contract == "a" && m.queued.iter().any(|q| q.worker.as_str() == worker))
}

/// The recipient `c` has read `src/a.rs` (its round-0 tool call is in its
/// transcript), so `a`'s report routes to it by subscription (0033).
async fn wait_for_c_read(p: &Parts) {
    wait_for("c's transcript holds its read of src/a.rs", || {
        std::fs::read_to_string(transcript_path(p, C_ATTEMPT))
            .is_ok_and(|t| t.contains("read_file") && t.contains("src/a.rs"))
    })
    .await;
}

fn messages_to_c<'a>(trace: &'a [TraceEvent], action: &str) -> Vec<(&'a str, i64, &'a str)> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Message {
                at_ms,
                id,
                action: a,
                to,
                detail,
                ..
            } if a == action && to.iter().any(|t| t == "c") => {
                Some((id.as_str(), *at_ms, detail.as_str()))
            }
            _ => None,
        })
        .collect()
}

fn posted_at(trace: &[TraceEvent], id: &str) -> i64 {
    trace
        .iter()
        .find_map(|e| match e {
            TraceEvent::Message {
                at_ms,
                id: i,
                action,
                ..
            } if action == "posted" && i == id => Some(*at_ms),
            _ => None,
        })
        .expect("the message was posted")
}

/// `c` is parked in its round-1 call when `a`'s report is posted. Released, the
/// call asks for one more tool, so a round boundary follows: the message is
/// injected there, quoted as data, and traced `delivered` with the time and
/// transcript sequence the runtime marked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_a_running_recipient_is_injected_at_its_next_round_boundary() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a HOLDA", "src/a.rs", &[]),
        c("c", "make c LATEC TOOLROUND READ src/a.rs", "src/c.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let hold_a = p.hold_workers("HOLDA");
    let hold_c = p.hold_workers_late("LATEC");
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and c".into()));

    wait_for("c parks in round 1", || p.late_parked()).await;
    wait_for_c_read(&p).await;
    hold_a.add_permits(1);
    // Saved after the copy was pushed to c's inbox and handed to its injector.
    wait_for("a's report is queued for c", || {
        queued_copy(&p, C_ATTEMPT).is_some()
    })
    .await;
    hold_c.add_permits(1);
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let record = try_record(&p).expect("record");
    let msg = record
        .coordination
        .mailbox
        .messages
        .iter()
        .find(|m| m.from_contract == "a")
        .expect("a's report");
    assert!(msg.delivered_to.contains("c"), "{msg:?}");
    let copy = msg
        .queued
        .iter()
        .find(|q| q.worker.as_str() == C_ATTEMPT)
        .expect("a copy for c's attempt");
    let CopyState::Injected { seq, at_ms } = copy.state else {
        panic!("the copy must be injected: {copy:?}");
    };

    // The runtime marked the inbox row with the same sequence and time.
    let row = queued_row_id(&msg.id, &AgentId::new(C_ATTEMPT));
    let item = SharedSessions(p.graph.clone())
        .inbox_item(&row)
        .expect("store")
        .expect("the copy's inbox row");
    assert_eq!(item.delivered, Some((seq, at_ms)));

    // Traced: pending at post, delivered at the boundary, never undelivered.
    let trace = trace_of(&p).await;
    let posted = posted_at(&trace, &msg.id);
    let pending = messages_to_c(&trace, "pending");
    assert!(
        pending.iter().any(|(id, at, d)| *id == msg.id
            && *at == posted
            && d.contains(&format!(
                "queued for {C_ATTEMPT} at its next round boundary"
            ))),
        "{pending:?}"
    );
    let delivered = messages_to_c(&trace, "delivered");
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    let (id, delivered_at, detail) = delivered[0];
    assert_eq!(id, msg.id);
    assert_eq!(delivered_at, at_ms, "the event carries the delivery time");
    assert!(delivered_at >= posted, "{delivered_at} < {posted}");
    assert!(
        detail.starts_with(&format!("injected into {C_ATTEMPT} at a round boundary")),
        "{detail}"
    );
    assert!(messages_to_c(&trace, "undelivered").is_empty());

    // In c's transcript: after the round-1 tool result (a boundary), quoted as
    // data with the message's id, and before the round that answers.
    let lines = transcript_lines(&p, C_ATTEMPT);
    let at = |pred: &dyn Fn(&serde_json::Value) -> bool| lines.iter().position(pred);
    let text = |v: &serde_json::Value| v.to_string();
    let injected = at(&|v| text(v).contains(&row)).expect("an injected event for the copy");
    let injected_text = text(&lines[injected]);
    assert!(
        injected_text.contains(&format!("agent_message id=\\\"{}\\\"", msg.id)),
        "{injected_text}"
    );
    assert!(
        injected_text.contains("not instructions to you"),
        "{injected_text}"
    );
    let tool_done = lines
        .iter()
        .rposition(|v| text(v).contains("t5"))
        .expect("the round-1 tool call");
    assert!(
        injected > tool_done,
        "injected at the boundary after the tool round"
    );
}

/// `c` is parked in round 1 when the report is posted, and that call then fails:
/// the attempt ends without a boundary, so its copy is never injected. The retry
/// is a fresh attempt whose brief carries the message instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_a_recipient_whose_attempt_ends_first_reaches_its_next_brief() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a HOLDA", "src/a.rs", &[]),
        c("c", "make c HANG READ src/a.rs", "src/c.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let hold_a = p.hold_workers("HOLDA");
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and c".into()));

    wait_for("c parks in round 1", || {
        p.alpha.hang_reached.load(Ordering::SeqCst) || p.beta.hang_reached.load(Ordering::SeqCst)
    })
    .await;
    wait_for_c_read(&p).await;
    hold_a.add_permits(1);
    wait_for("a's report is queued for c", || {
        queued_copy(&p, C_ATTEMPT).is_some()
    })
    .await;
    // The retry must not park: only the call parked now fails.
    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    p.alpha.fail_parked.store(true, Ordering::SeqCst);
    p.beta.fail_parked.store(true, Ordering::SeqCst);
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let record = try_record(&p).expect("record");
    let msg = record
        .coordination
        .mailbox
        .messages
        .iter()
        .find(|m| m.from_contract == "a")
        .expect("a's report");
    assert!(msg.delivered_to.contains("c"), "{msg:?}");
    let copy = msg
        .queued
        .iter()
        .find(|q| q.worker.as_str() == C_ATTEMPT)
        .expect("a copy for c's first attempt");
    assert_eq!(copy.state, CopyState::NotReached, "{copy:?}");
    // The copy stays in the first attempt's inbox, undelivered, as the record
    // of what it never reached.
    let row = queued_row_id(&msg.id, &AgentId::new(C_ATTEMPT));
    let item = SharedSessions(p.graph.clone())
        .inbox_item(&row)
        .expect("store")
        .expect("the copy's inbox row");
    assert_eq!(item.delivered, None);

    let trace = trace_of(&p).await;
    let pending = messages_to_c(&trace, "pending");
    assert!(
        pending
            .iter()
            .any(|(id, _, d)| *id == msg.id && d.contains(&format!("queued for {C_ATTEMPT}"))),
        "{pending:?}"
    );
    let delivered = messages_to_c(&trace, "delivered");
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    let (id, at, detail) = delivered[0];
    assert_eq!(id, msg.id);
    assert!(at >= posted_at(&trace, &msg.id));
    let retry = detail
        .strip_prefix("in the brief of ")
        .unwrap_or_else(|| panic!("delivered by a brief: {detail}"));
    assert_ne!(retry, C_ATTEMPT, "a fresh attempt's brief");
    assert!(messages_to_c(&trace, "undelivered").is_empty());
    // The retry's opening carries it once, quoted as data.
    let tag = format!("<agent_message id=\"{}\"", msg.id);
    let openings = p.stats.openings.lock().expect("lock").clone();
    let carrying: Vec<&String> = openings.iter().filter(|o| o.contains(&tag)).collect();
    assert_eq!(carrying.len(), 1, "{openings:?}");
    assert_eq!(carrying[0].matches(&tag).count(), 1);
}

/// `c` is parked in its last round (round 1 answers) when the report is posted:
/// no boundary follows, and `c` is verified and closes with no fresh brief. The
/// message is traced `undelivered` and reported to its sender, `a`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_never_delivered_is_traced_and_reported_to_its_sender() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a HOLDA", "src/a.rs", &[]),
        c("c", "make c LATEC READ src/a.rs", "src/c.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let hold_a = p.hold_workers("HOLDA");
    let hold_c = p.hold_workers_late("LATEC");
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_session_notices(Arc::new(move |t: String| {
            sink.lock().expect("lock").push(t);
        }));
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and c".into()));

    wait_for("c parks in round 1", || p.late_parked()).await;
    wait_for_c_read(&p).await;
    hold_a.add_permits(1);
    wait_for("a's report is queued for c", || {
        queued_copy(&p, C_ATTEMPT).is_some()
    })
    .await;
    hold_c.add_permits(1);
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let record = try_record(&p).expect("record");
    let msg = record
        .coordination
        .mailbox
        .messages
        .iter()
        .find(|m| m.from_contract == "a")
        .expect("a's report");
    assert!(!msg.delivered_to.contains("c"), "{msg:?}");
    let copy = msg
        .queued
        .iter()
        .find(|q| q.worker.as_str() == C_ATTEMPT)
        .expect("a copy for c's attempt");
    assert_eq!(copy.state, CopyState::NotReached, "{copy:?}");

    let trace = trace_of(&p).await;
    assert!(messages_to_c(&trace, "delivered").is_empty());
    let undelivered = messages_to_c(&trace, "undelivered");
    assert_eq!(undelivered.len(), 1, "{undelivered:?}");
    let (id, at, detail) = undelivered[0];
    assert_eq!(id, msg.id);
    assert!(at >= posted_at(&trace, &msg.id));
    assert!(detail.contains("c closed (verified)"), "{detail}");
    // Reported to the sender: a notice on a's record, the session sink and the
    // run report.
    let reported = |t: &String| t.contains(&msg.id) && t.contains("not delivered");
    assert!(
        trace.iter().any(|e| matches!(
            e,
            TraceEvent::Notice { contract, detail, .. } if contract == "a" && reported(detail)
        )),
        "a notice on the sender's record"
    );
    let seen = seen.lock().expect("lock").clone();
    assert!(seen.iter().any(reported), "{seen:?}");
    assert!(report.notices.iter().any(reported), "{:?}", report.notices);
}
