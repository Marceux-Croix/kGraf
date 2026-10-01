//! A route whose calls fail at the transport is named, counted and left
//! behind; its contracts move to a healthy route without counting the failed
//! attempt; with no other route the contract waits for the route's probe and
//! is held with the reason once the route has failed three times in a row.
//!
//! The round-1 canary's route-outage scenario saw three calls dropped on one
//! route while neither the trace nor the run output named the route, and
//! rotation kept handing it new contracts.

mod support;

use kgraf_core::{RunId, TokenUsage};
use kgraf_orchestrator::{Orchestrator, RunStatus, TraceEvent, TraceLog};
use kgraf_provider::{CallToken, Provider, ProviderError, Request, Response};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A route whose server drops every connection: each call fails with a
/// transport error, closed in the ledger the way an adapter closes it.
#[derive(Default)]
struct DownFake {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for DownFake {
    fn adapter(&self) -> &'static str {
        "fake-down"
    }

    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }

    async fn complete(
        &self,
        _request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let error = ProviderError::Transport {
            adapter: "fake-down",
            reason: "error sending request for url (http://localhost:8080/v1/chat/completions)"
                .into(),
        };
        token.close_on(&error, None)?;
        let _ = TokenUsage::default();
        Err(error)
    }
}

async fn trace(p: &Parts) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

fn services_with_down(
    p: &Parts,
    down: Arc<DownFake>,
    healthy: bool,
) -> kgraf_orchestrator::Services {
    let mut s = p.services(WINDOW);
    // Route 0 (the first rotation pick) is down; route 1, when kept, answers.
    s.routes[0] = route(
        "down",
        "vendor-d",
        ALPHA_MODEL,
        down,
        p.ledger.clone(),
        WINDOW,
    );
    if !healthy {
        s.routes.truncate(1);
    }
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_failing_at_the_transport_is_named_and_its_contracts_move_off_it() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("c1", "one", "src/a.rs", &[]),
        c("c2", "two", "src/b.rs", &[]),
        c("c3", "three", "src/c.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let down = Arc::new(DownFake::default());
    let mut cfg = config();
    // No retries: a route fault counted as the contract's failure would hold it.
    cfg.max_retries = 0;
    let report = Orchestrator::new(cfg, services_with_down(&p, down.clone(), true))
        .expect("orchestrator")
        .start(RunId::new("r1"), "three files".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified.len(), 3, "{report:?}");
    let events = trace(&p).await;
    // Contracts dispatched before the first fault came back may have gone to
    // the down route; none is dispatched to it after.
    let first_fault = events
        .iter()
        .position(|e| matches!(e, TraceEvent::RouteFault { .. }))
        .expect("a fault");
    let dispatched_to_down_before = events[..first_fault]
        .iter()
        .filter(|e| matches!(e, TraceEvent::Dispatch { route, .. } if route == "down"))
        .count();
    assert!(
        !events[first_fault..]
            .iter()
            .any(|e| matches!(e, TraceEvent::Dispatch { route, .. } if route == "down")),
        "the down route is out of rotation after its first fault: {events:?}"
    );
    assert_eq!(down.calls.load(Ordering::SeqCst), dispatched_to_down_before);
    let fault = events
        .iter()
        .find_map(|e| match e {
            TraceEvent::RouteFault {
                route,
                total,
                detail,
                ..
            } => Some((route.clone(), *total, detail.clone())),
            _ => None,
        })
        .expect("the fault is traced");
    assert_eq!(fault.0, "down");
    assert_eq!(fault.1, 1);
    assert!(fault.2.contains("transport failure"), "{}", fault.2);
    assert!(
        events.iter().any(|e| matches!(
            e,
            TraceEvent::Rerouted { from, to, .. } if from == "down" && to == "beta"
        )),
        "the move off the route is traced: {events:?}"
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("route `down`") && n.contains("failed at the transport")),
        "the run output names the route: {:?}",
        report.notices
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_no_other_route_the_contract_waits_for_probes_then_is_held_with_the_reason() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("c1", "one", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let down = Arc::new(DownFake::default());
    let mut cfg = config();
    cfg.max_retries = 0;
    cfg.route_fault_cooldown = Duration::from_millis(20);
    let report = Orchestrator::new(cfg, services_with_down(&p, down.clone(), false))
        .expect("orchestrator")
        .start(RunId::new("r1"), "one file".into())
        .await
        .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    assert_eq!(
        down.calls.load(Ordering::SeqCst),
        3,
        "probed after each cooldown, held after the third fault in a row"
    );
    let (_, reason) = report.held.first().expect("held");
    assert!(reason.contains("route `down`"), "{reason}");
    assert!(reason.contains("3 times in a row"), "{reason}");
    assert!(reason.contains("transport"), "{reason}");
}
