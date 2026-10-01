//! The run loop's side of progress-gated round budgets (decision 0096; the
//! mechanism is `kgraf_runtime`'s progress gate).
//!
//! `record_checkpoints`, when a worker finishes: every checkpoint it reached
//! is written to the trace (`worker_checkpoints`). A worker that ran past its
//! round budget on measured progress is the orchestrator's evidence that the
//! contract was bigger than planned, the same signal as a hand-back or a
//! stop-point compaction: it is counted once as a sizing miss (not again when
//! the worker also handed back or compacted), counted in `round_extensions`,
//! and reported once. The ledger already holds the worker's rounds and peak
//! context, which is what the sizer projects later plans from. A worker a
//! checkpoint stopped is counted in `checkpoint_stops`; its outcome is the
//! runtime's `BudgetExhausted`, handled as before (a failed live outcome and a
//! stuck report, retried, then held), and the stuck report names why.

use super::*;
use kgraf_runtime::{CheckpointVerdict, RoundBlocks};

impl RunLoop {
    /// Record a worker's round checkpoints: trace, sizing miss, counters,
    /// report. `counted`: a hand-back or compaction already counted this
    /// worker's sizing miss.
    pub(super) async fn record_checkpoints(
        &mut self,
        contract: &OpenContract,
        worker: &AgentId,
        blocks: &RoundBlocks,
        counted: bool,
        now: i64,
    ) -> Result<(), OrchestratorError> {
        let extended = blocks.extended();
        self.trace(TraceEvent::WorkerCheckpoints {
            at_ms: now,
            contract: contract.contract.clone(),
            worker: worker.to_string(),
            extended,
            blocks: blocks.clone(),
        })
        .await?;
        if matches!(
            blocks.ended_by(),
            Some(CheckpointVerdict::EndStuck | CheckpointVerdict::Ceiling)
        ) {
            self.record.counters.checkpoint_stops += 1;
        }
        if !extended {
            return Ok(());
        }
        self.record.counters.round_extensions += 1;
        if !counted {
            self.record.counters.sizing_misses += 1;
        }
        let granted: u32 = blocks.checkpoints.iter().map(|c| c.rounds_granted).sum();
        let text = format!(
            "ran past its round budget of {} on measured progress ({} more rounds over {} \
             checkpoint(s)): the contract was larger than planned{}",
            blocks.block_rounds,
            granted,
            blocks.checkpoints.len(),
            checkpoint_note(Some(blocks))
        );
        self.submit_report(contract, Some(worker), ReportKind::Informational, &text);
        Ok(())
    }
}

/// What a report adds about the checkpoint that ended a worker, if one did.
pub(super) fn checkpoint_note(blocks: Option<&RoundBlocks>) -> String {
    let Some(blocks) = blocks else {
        return String::new();
    };
    let at = blocks.checkpoints.last().map_or(0, |c| c.at_round);
    match blocks.ended_by() {
        Some(CheckpointVerdict::EndStuck) => format!(
            "; stopped at round {at}: no progress at two checkpoints in a row (no net file \
             change, no improving check, or a repeating failure)"
        ),
        Some(CheckpointVerdict::Ceiling) => format!(
            "; stopped at round {at}: the checkpoint ceiling of {} was reached",
            blocks.ceiling
        ),
        Some(CheckpointVerdict::EndNoProgress) => format!(
            "; no progress was measured by round {at}, so the round budget ended the attempt"
        ),
        Some(CheckpointVerdict::Continue | CheckpointVerdict::Grace) | None => String::new(),
    }
}
