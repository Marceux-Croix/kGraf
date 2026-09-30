//! Worker report delivery policy (decisions 0009, 0027).
//!
//! Ported from Untrivial-ai/agent-orchestrator, commit
//! 73473d45f0c18f3a81f66f150868459e3098ca35, Apache-2.0 (license read at the
//! source 2026-09-26; no NOTICE file exists at that commit; see
//! `third_party/agent-orchestrator/UPSTREAM`):
//!
//! - `backend/internal/domain/report.go`: `MaxReportTextCharacters` (1,000),
//!   `ReportBatchFallback` (one hour), `ReportSettlementWindow` (five minutes);
//! - `backend/internal/service/report/service.go` `Create`: when each kind becomes
//!   available;
//! - `backend/internal/service/report/coordinator.go` `deliverProject`: stuck
//!   reports interrupt at most once per worker per `ReportInterruptWindow` (three
//!   minutes), and a batch whose only urgency is a stuck report inside that window
//!   is coalesced until the window opens.
//!
//! | Kind | Available | Recipients |
//! |---|---|---|
//! | informational | within one hour, earlier with any other delivery | orchestrator |
//! | needs-input | now | orchestrator |
//! | stuck | now, plus a rate-limited interrupt | orchestrator |
//! | done | after a five-minute settlement window | orchestrator |
//! | contract-inadequate | now | orchestrator **and every peer holding the contract** |
//!
//! The fifth kind is kGraf's (0027): a bad contract affects every worker holding
//! it, so it is the only report that fans out. Upstream's durable outbox (claim
//! tokens, acknowledgment) is not ported: this queue lives inside the orchestrator
//! loop, which is the one consumer, and a report is a trace record once delivered.

use kgraf_core::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Upstream `MaxReportTextCharacters`.
pub const MAX_REPORT_CHARS: usize = 1_000;
/// Upstream `ReportBatchFallback`.
pub const BATCH_FALLBACK_MS: i64 = 60 * 60 * 1_000;
/// Upstream `ReportSettlementWindow`.
pub const SETTLEMENT_WINDOW_MS: i64 = 5 * 60 * 1_000;
/// Upstream `ReportInterruptWindow`.
pub const INTERRUPT_WINDOW_MS: i64 = 3 * 60 * 1_000;

/// The five report kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Informational,
    NeedsInput,
    Stuck,
    Done,
    ContractInadequate,
}

/// One report from a worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerReport {
    pub worker: AgentId,
    pub contract: String,
    pub kind: ReportKind,
    pub note: String,
    pub created_ms: i64,
}

/// A report that cannot be queued.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReportError {
    #[error("a report needs a note")]
    EmptyNote,
    #[error("the note is {len} characters; the limit is {max}")]
    TooLong { len: usize, max: usize },
}

/// Who a delivery goes to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recipient {
    Orchestrator,
    Peer(AgentId),
}

/// One delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub recipients: Vec<Recipient>,
    pub reports: Vec<WorkerReport>,
    /// Whether this delivery interrupts the recipient's active work.
    pub interrupt: bool,
}

#[derive(Clone, Debug)]
struct Pending {
    report: WorkerReport,
    available_ms: i64,
    peers: Vec<AgentId>,
}

/// The queue between workers and the orchestrator.
#[derive(Clone, Debug, Default)]
pub struct ReportQueue {
    pending: Vec<Pending>,
    last_interrupt: BTreeMap<AgentId, i64>,
    /// A coalesced stuck batch waits until this time.
    deferred_until: Option<i64>,
}

impl ReportQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a report. `peers` are the other workers holding the contract; only a
    /// contract-inadequate report uses them.
    pub fn submit(&mut self, report: WorkerReport, peers: &[AgentId]) -> Result<(), ReportError> {
        if report.note.trim().is_empty() {
            return Err(ReportError::EmptyNote);
        }
        let len = report.note.chars().count();
        if len > MAX_REPORT_CHARS {
            return Err(ReportError::TooLong {
                len,
                max: MAX_REPORT_CHARS,
            });
        }
        let now = report.created_ms;
        let available_ms = match report.kind {
            ReportKind::Done => now + SETTLEMENT_WINDOW_MS,
            ReportKind::NeedsInput | ReportKind::Stuck | ReportKind::ContractInadequate => now,
            ReportKind::Informational => now + BATCH_FALLBACK_MS,
        };
        // A new urgent report is a new reason to deliver; it lifts a coalesced wait.
        if matches!(
            report.kind,
            ReportKind::NeedsInput | ReportKind::ContractInadequate
        ) {
            self.deferred_until = None;
        }
        self.pending.push(Pending {
            report,
            available_ms,
            peers: peers
                .iter()
                .filter(|p| **p != AgentId::new(""))
                .cloned()
                .collect(),
        });
        Ok(())
    }

    /// Reports not yet delivered.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Everything due at `now`, as deliveries.
    pub fn due(&mut self, now: i64) -> Vec<Delivery> {
        if self.deferred_until.is_some_and(|d| now < d) {
            return Vec::new();
        }
        self.deferred_until = None;
        if !self.pending.iter().any(|p| p.available_ms <= now) {
            return Vec::new();
        }
        let mut out = Vec::new();

        // Fan-out first: each contract-inadequate report also goes to its peers.
        let (fan, rest): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| {
                p.report.kind == ReportKind::ContractInadequate && p.available_ms <= now
            });
        self.pending = rest;
        for p in fan {
            let mut recipients = vec![Recipient::Orchestrator];
            recipients.extend(p.peers.iter().cloned().map(Recipient::Peer));
            out.push(Delivery {
                recipients,
                reports: vec![p.report],
                interrupt: false,
            });
        }

        // The orchestrator batch: everything available, plus informational reports
        // riding along early. A done report waits out its settlement window.
        let batch_ready = self.pending.iter().any(|p| p.available_ms <= now);
        if !batch_ready {
            return out;
        }
        let (batch, rest): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| p.available_ms <= now || p.report.kind == ReportKind::Informational);
        self.pending = rest;

        let has = |kind: ReportKind| batch.iter().any(|p| p.report.kind == kind);
        let mut interrupt = false;
        if has(ReportKind::Stuck) {
            let mut next_interrupt: Option<i64> = None;
            let stuck: Vec<AgentId> = batch
                .iter()
                .filter(|p| p.report.kind == ReportKind::Stuck)
                .map(|p| p.report.worker.clone())
                .collect();
            for worker in stuck {
                let last = self.last_interrupt.get(&worker).copied();
                match last {
                    Some(at) if now < at + INTERRUPT_WINDOW_MS => {
                        let next = at + INTERRUPT_WINDOW_MS;
                        next_interrupt = Some(next_interrupt.map_or(next, |n| n.min(next)));
                    }
                    _ => {
                        self.last_interrupt.insert(worker, now);
                        interrupt = true;
                    }
                }
            }
            if !interrupt && !has(ReportKind::NeedsInput) {
                // Upstream: "repeated stuck report coalesced inside interrupt window".
                self.pending.extend(batch);
                self.deferred_until = next_interrupt;
                return out;
            }
        }
        out.push(Delivery {
            recipients: vec![Recipient::Orchestrator],
            reports: batch.into_iter().map(|p| p.report).collect(),
            interrupt,
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(worker: &str, kind: ReportKind, at: i64) -> WorkerReport {
        WorkerReport {
            worker: AgentId::new(worker),
            contract: "c1".into(),
            kind,
            note: format!("{kind:?} from {worker}"),
            created_ms: at,
        }
    }

    #[test]
    fn informational_waits_up_to_an_hour_and_rides_along_with_urgent_reports() {
        let mut q = ReportQueue::new();
        q.submit(report("w1", ReportKind::Informational, 0), &[])
            .expect("ok");
        assert!(q.due(1_000).is_empty());
        q.submit(report("w2", ReportKind::NeedsInput, 2_000), &[])
            .expect("ok");
        let out = q.due(2_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reports.len(), 2, "informational rode along");
        assert!(!out[0].interrupt);
        assert_eq!(q.pending(), 0);

        let mut q = ReportQueue::new();
        q.submit(report("w1", ReportKind::Informational, 0), &[])
            .expect("ok");
        assert_eq!(q.due(BATCH_FALLBACK_MS).len(), 1, "fallback deadline");
    }

    #[test]
    fn done_opens_a_settlement_window_and_does_not_ride_along() {
        let mut q = ReportQueue::new();
        q.submit(report("w1", ReportKind::Done, 0), &[])
            .expect("ok");
        q.submit(report("w2", ReportKind::NeedsInput, 10), &[])
            .expect("ok");
        let out = q.due(10);
        assert_eq!(out[0].reports.len(), 1);
        assert_eq!(out[0].reports[0].kind, ReportKind::NeedsInput);
        assert!(q.due(SETTLEMENT_WINDOW_MS - 1).is_empty());
        let out = q.due(SETTLEMENT_WINDOW_MS);
        assert_eq!(out[0].reports[0].kind, ReportKind::Done);
    }

    #[test]
    fn stuck_interrupts_once_per_window_and_repeats_are_coalesced() {
        let mut q = ReportQueue::new();
        q.submit(report("w1", ReportKind::Stuck, 0), &[])
            .expect("ok");
        let out = q.due(0);
        assert!(out[0].interrupt);
        q.submit(report("w1", ReportKind::Stuck, 1_000), &[])
            .expect("ok");
        assert!(q.due(1_000).is_empty(), "coalesced inside the window");
        let out = q.due(INTERRUPT_WINDOW_MS);
        assert!(out[0].interrupt);
        // A different worker's stuck report is not rate-limited by w1's window.
        q.submit(
            report("w2", ReportKind::Stuck, INTERRUPT_WINDOW_MS + 1),
            &[],
        )
        .expect("ok");
        assert!(q.due(INTERRUPT_WINDOW_MS + 1)[0].interrupt);
    }

    #[test]
    fn a_repeated_stuck_report_is_delivered_without_interrupt_when_input_is_needed() {
        let mut q = ReportQueue::new();
        q.submit(report("w1", ReportKind::Stuck, 0), &[])
            .expect("ok");
        q.due(0);
        q.submit(report("w1", ReportKind::Stuck, 5), &[])
            .expect("ok");
        q.submit(report("w1", ReportKind::NeedsInput, 6), &[])
            .expect("ok");
        let out = q.due(6);
        assert_eq!(out[0].reports.len(), 2);
        assert!(!out[0].interrupt);
    }

    #[test]
    fn contract_inadequate_is_the_only_kind_that_fans_out_to_peers() {
        let mut q = ReportQueue::new();
        let peers = [AgentId::new("w2"), AgentId::new("w3")];
        q.submit(report("w1", ReportKind::ContractInadequate, 0), &peers)
            .expect("ok");
        q.submit(report("w1", ReportKind::NeedsInput, 0), &peers)
            .expect("ok");
        let out = q.due(0);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].recipients,
            vec![
                Recipient::Orchestrator,
                Recipient::Peer(AgentId::new("w2")),
                Recipient::Peer(AgentId::new("w3"))
            ]
        );
        assert_eq!(out[1].recipients, vec![Recipient::Orchestrator]);
    }

    #[test]
    fn notes_are_bounded_and_required() {
        let mut q = ReportQueue::new();
        let mut r = report("w1", ReportKind::Stuck, 0);
        r.note = " ".into();
        assert_eq!(q.submit(r.clone(), &[]), Err(ReportError::EmptyNote));
        r.note = "x".repeat(MAX_REPORT_CHARS + 1);
        assert!(matches!(q.submit(r, &[]), Err(ReportError::TooLong { .. })));
    }
}
