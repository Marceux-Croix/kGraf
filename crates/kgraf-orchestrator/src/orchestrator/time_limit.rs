//! The run loop's side of the run's time limit (`run_deadline.rs`): the
//! deadline each attempt is given, the gate before an attempt starts, and the
//! notice and trace record of each stop and refusal.
//!
//! A child module of `orchestrator.rs`, like `coord.rs`, so the loop carries
//! one-line hooks.

use super::*;
use crate::run_deadline::secs;

impl RunLoop {
    /// The deadline of an attempt starting now on `route`. `None`: the run
    /// has no time limit.
    pub(super) fn attempt_deadline(&self, route: &str) -> Option<kgraf_runtime::WorkerDeadline> {
        self.run_deadline
            .as_ref()
            .map(|d| d.worker_deadline(self.now(), route))
    }

    /// Whether an attempt of `contract` may start now. `false`: the time left
    /// before the worker deadline is below the floor one round needs; a
    /// notice (once per contract) and a trace record say so, and the run ends
    /// with what it has. Always `true` without a time limit.
    pub(super) async fn time_for_attempt(
        &mut self,
        contract: &str,
    ) -> Result<bool, OrchestratorError> {
        let now = self.now();
        let Some(Err(no)) = self.run_deadline.as_ref().map(|d| d.may_start(now)) else {
            return Ok(true);
        };
        let why = no.describe();
        // One notice and one record per contract: the dispatch round asks
        // again each tick, and the numbers in the text move with the clock.
        if self.time_refused.insert(contract.to_owned()) {
            self.notices
                .insert(kgraf_secrets::scrub_persist_owned(format!(
                    "contract {contract}: no attempt was started at the time limit: {why}"
                )));
            self.trace(TraceEvent::TimeLimit {
                at_ms: now,
                contract: contract.to_owned(),
                worker: String::new(),
                action: "attempt_refused".to_owned(),
                limit_ms: no.limit_ms,
                deadline_ms: no.deadline_ms,
                elapsed_ms: no.elapsed_ms,
                remaining_ms: no.left_ms,
                last_round_ms: None,
                expected_round_ms: Some(no.floor_ms),
                cut: false,
            })
            .await?;
        }
        self.out_of_time = Some(format!("no attempt could start at the time limit: {why}"));
        Ok(false)
    }

    /// The refusal of a continuation (a hand-back's re-split) for lack of
    /// time, so the handed-back tree is verified as it stands instead of
    /// waiting on children that will not start.
    pub(super) fn no_time_for_continuation(&self) -> Option<String> {
        let d = self.run_deadline.as_ref()?;
        d.may_start(self.now())
            .err()
            .map(|no| format!("no time is left for a continuation: {}", no.describe()))
    }

    /// A worker ended: its round measure seeds the next worker on the route
    /// and sets the floor; a stop at its deadline is a notice and a trace
    /// record with the numbers.
    pub(super) async fn note_worker_time(
        &mut self,
        contract: &str,
        worker: &AgentId,
        route: &str,
        result: &WorkerResult,
        now: i64,
    ) -> Result<(), OrchestratorError> {
        let Some(d) = self.run_deadline.as_mut() else {
            return Ok(());
        };
        if let Some(ms) = result.recent_round_ms {
            d.record_round(route, ms);
        }
        let Some(stop) = result.out_of_time else {
            return Ok(());
        };
        let (limit_ms, deadline_ms, elapsed_ms) =
            (d.limit_ms(), d.deadline_ms(), d.elapsed_ms(now));
        let mut text = format!(
            "contract {contract}: worker {worker} stopped at its time limit {} s into the run's \
             {} s limit, {} s before its deadline at {} s",
            secs(elapsed_ms),
            secs(limit_ms),
            secs(stop.remaining_ms),
            secs(deadline_ms)
        );
        if let Some(last) = stop.last_round_ms {
            text.push_str(&format!("; its last round took {} s", secs(last)));
        }
        if let Some(expected) = stop.expected_round_ms {
            text.push_str(&format!(
                "; the next was expected to take {} s",
                secs(expected)
            ));
        }
        if stop.cut {
            text.push_str("; the round in flight was cut and its call settled as cancelled");
        }
        text.push_str(if result.changed_paths.is_empty() {
            "; it changed no file"
        } else {
            "; its tree is verified as it stands"
        });
        if !result.changed_paths.is_empty() && d.may_start(now).is_err() {
            text.push_str(", because no further attempt could run");
        }
        self.notices
            .insert(kgraf_secrets::scrub_persist_owned(text));
        self.trace(TraceEvent::TimeLimit {
            at_ms: now,
            contract: contract.to_owned(),
            worker: worker.to_string(),
            action: "worker_stopped".to_owned(),
            limit_ms,
            deadline_ms,
            elapsed_ms,
            remaining_ms: stop.remaining_ms,
            last_round_ms: stop.last_round_ms,
            expected_round_ms: stop.expected_round_ms,
            cut: stop.cut,
        })
        .await
    }

    /// A stopped worker's tree is verified because no further attempt can
    /// run: a notice and a trace record (`tree_verified`) say so, with the
    /// time left to the limit for the check, the merge and the gate.
    pub(super) async fn note_limit_verify(
        &mut self,
        contract: &str,
        worker: &AgentId,
        now: i64,
    ) -> Result<(), OrchestratorError> {
        let Some(d) = self.run_deadline.as_ref() else {
            return Ok(());
        };
        let (limit_ms, deadline_ms, elapsed_ms, left_ms) = (
            d.limit_ms(),
            d.deadline_ms(),
            d.elapsed_ms(now),
            d.limit_left_ms(now),
        );
        self.notices
            .insert(kgraf_secrets::scrub_persist_owned(format!(
                "contract {contract}: worker {worker} was stopped at the run's time limit and no \
                 further attempt could run, so its tree is verified as it stands, inside the {} s \
                 left before the limit",
                secs(left_ms)
            )));
        self.trace(TraceEvent::TimeLimit {
            at_ms: now,
            contract: contract.to_owned(),
            worker: worker.to_string(),
            action: "tree_verified".to_owned(),
            limit_ms,
            deadline_ms,
            elapsed_ms,
            remaining_ms: left_ms,
            last_round_ms: None,
            expected_round_ms: None,
            cut: false,
        })
        .await
    }
}
