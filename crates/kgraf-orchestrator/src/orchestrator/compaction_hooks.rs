//! The run loop's side of runtime compaction (decision 0095; the mechanism is
//! `kgraf_runtime::compaction`).
//!
//! 1. `stop_point_policy`, at dispatch: the remainder the worker would hand
//!    back is put to the delegation guards as a would-be child. When they
//!    would refuse it (the depth cap, the task length bound, a taken child
//!    id), a hand-back would only hold the contract, so the worker is spawned
//!    with `StopPointPolicy::Compact`: it compacts once and goes on. Where the
//!    guards would pass it, the worker hands back as before.
//! 2. `record_compaction`, when the worker finishes: the runtime's notice is
//!    the orchestrator's evidence that the contract was larger than planned.
//!    It is written to the trace (`worker_compacted`) and the run record
//!    (`compactions`), counted as a sizing miss, and reported. The ledger
//!    already holds the worker's peak context and the summary call, which is
//!    what the sizer projects later plans from.
//! 3. `compaction_note`: a compacted worker that overflowed again hands back;
//!    when the re-split is refused, the held reason says where the work and
//!    the full transcript are, so nothing is lost.
//! 4. `request_compaction`: `/compact <worker>` reaches the running worker's
//!    queue; the worker compacts at its next boundary.

use super::*;
use kgraf_runtime::{CompactRequestError, CompactionNotice, CompactionTrigger, StopPointPolicy};

impl RunLoop {
    /// Whether a hand-back of `contract`, briefed with `instruction`, could be
    /// split. The would-be child is worded as the runtime words a hand-back.
    pub(super) fn stop_point_policy(
        &self,
        contract: &OpenContract,
        instruction: &str,
        stop_tokens: u64,
    ) -> StopPointPolicy {
        let roster = self.roster();
        let Some(first) = roster.first() else {
            return StopPointPolicy::Compact;
        };
        let source_route = contract.route.clone().unwrap_or_else(|| first.clone());
        let task = kgraf_runtime::handback_remainder(instruction, stop_tokens, stop_tokens);
        let child = format!("{}.h1", contract.contract);
        let taken = self.record.contract(&child).is_some()
            || self.record.verified_contracts.contains(&child)
            || self.record.split_contracts.contains(&child);
        let passes = guards::check(
            &self.delegation_policy(),
            &roster,
            &DelegationRequest {
                source_contract: &contract.contract,
                source_route: &source_route,
                target_route: &source_route,
                task: &task,
                trace: &contract.lineage,
                trace_tasks: &contract.lineage_tasks,
            },
        )
        .is_ok();
        if passes && !taken {
            StopPointPolicy::HandBack
        } else {
            StopPointPolicy::Compact
        }
    }

    /// Record a worker's compaction: trace, run record, sizing miss, report.
    pub(super) async fn record_compaction(
        &mut self,
        contract: &OpenContract,
        worker: &AgentId,
        notice: &CompactionNotice,
    ) -> Result<(), OrchestratorError> {
        self.trace(TraceEvent::WorkerCompacted {
            at_ms: notice.at_ms,
            contract: contract.contract.clone(),
            worker: worker.to_string(),
            notice: notice.clone(),
        })
        .await?;
        // A user's request says nothing about the plan; the stop point does.
        let sizing_miss = notice.trigger == CompactionTrigger::StopPoint;
        if sizing_miss {
            self.record.counters.sizing_misses += 1;
        }
        self.record
            .compactions
            .push(crate::run_state::CompactionRecord {
                contract: contract.contract.clone(),
                worker: worker.to_string(),
                sizing_miss,
                notice: notice.clone(),
            });
        let text = if sizing_miss {
            format!(
                "compacted once at {} of {} tokens: the contract was larger than planned; {}",
                notice.at_tokens,
                notice.window_tokens,
                transcript_words(notice)
            )
        } else {
            format!(
                "compacted once on request at {} tokens; {}",
                notice.at_tokens,
                transcript_words(notice)
            )
        };
        self.submit_report(contract, Some(worker), ReportKind::Informational, &text);
        Ok(())
    }

    /// What a held reason adds when the hand-back came from a worker that had
    /// already compacted: where its work and its full history are.
    pub(super) fn compaction_note(&self, worker: &AgentId) -> String {
        match self
            .record
            .compactions
            .iter()
            .rev()
            .find(|c| c.worker == worker.as_str())
        {
            Some(c) => format!(
                "; the worker had already compacted once at {} tokens and reached its stop point \
                 again; its work so far is kept in the worktree, and {}",
                c.notice.at_tokens,
                transcript_words(&c.notice)
            ),
            None => String::new(),
        }
    }

    /// `/compact <worker>`: ask the running worker to compact at its next
    /// boundary.
    pub(super) fn request_compaction(&self, worker: String) -> Result<(), CompactError> {
        let slot = self.in_flight.values().find_map(|f| {
            f.injectors
                .iter()
                .find(|(w, _)| w.as_str() == worker)
                .map(|(_, slot)| slot.clone())
        });
        let Some(slot) = slot else {
            return Err(CompactError::UnknownWorker(worker));
        };
        let Some(injector) = slot.get() else {
            return Err(CompactError::NotStarted(worker));
        };
        injector.request_compaction().map_err(|e| match e {
            CompactRequestError::Finished { .. } => CompactError::UnknownWorker(worker),
            CompactRequestError::AlreadyCompacted { .. } => CompactError::AlreadyCompacted(worker),
        })
    }
}

/// Where the pre-compaction history is, in words.
fn transcript_words(notice: &CompactionNotice) -> String {
    match &notice.transcript_path {
        Some(path) => format!(
            "the full transcript before compaction is kept in {}",
            path.display()
        ),
        None => "the full transcript before compaction is kept in the worker's transcript".into(),
    }
}
