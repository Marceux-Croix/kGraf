//! The run loop's side of a shortfall report (decision 0093; see
//! `crate::shortfall` for the rules).
//!
//! 1. `on_shortfall`: the report arrives while the worker runs. The
//!    orchestrator decides at once (grant, amend or hold), records the
//!    decision on the contract and in the trace, answers the worker, then
//!    cancels the attempt so the worker stops rather than guesses.
//! 2. `take_shortfall`: when that worker finishes, its attempt is marked
//!    stopped (not a candidate, not a failure) and its worker id retired, so
//!    the next attempt is a fresh worker built with the grant.
//! 3. `finalize_ordinary` applies a held decision as the contract's held
//!    reason, once every attempt has finished.

use super::*;
use crate::challenge::ContractReply;
use crate::shortfall::{ShortfallDecision, ShortfallKind, decide, stored};

impl RunLoop {
    pub(super) async fn on_shortfall(
        &mut self,
        worker: AgentId,
        kind: ShortfallKind,
        need: String,
        why: String,
    ) -> Result<ContractReply, OrchestratorError> {
        let Some(contract_id) = self
            .in_flight
            .iter()
            .find(|(_, f)| f.attempts.iter().any(|a| a.worker == worker))
            .map(|(c, _)| c.clone())
        else {
            return Ok(ContractReply::Refused(format!(
                "worker `{worker}` is not running an attempt of this run"
            )));
        };
        let Some(contract) = self.record.contract(&contract_id).cloned() else {
            return Ok(ContractReply::Refused(format!(
                "contract `{contract_id}` is not open"
            )));
        };
        // One report per attempt: the attempt is stopping.
        if contract.shortfall_workers.contains(&worker) {
            return Ok(ContractReply::Refused(
                "this attempt already reported a shortfall; stop now".into(),
            ));
        }
        let (need, why) = (stored(&need), stored(&why));
        let decision = decide(kind, &need, &why, &contract, &self.config.grant_ceiling);
        // Decision 0093: a grant reaches a fresh attempt only through an
        // environment builder that applies it. Without one the grant could
        // not take effect, so it is held rather than silently dropped.
        let decision = match decision {
            ShortfallDecision::Grant { program } if self.attempt_envs.is_none() => {
                ShortfallDecision::Hold {
                    reason: format!(
                        "`{program}` is within the operator's ceiling, but this run builds no \
                         per-attempt environment that could apply a grant"
                    ),
                }
            }
            other => other,
        };
        let now = self.now();
        if let Some(c) = self.record.contract_mut(&contract_id) {
            c.shortfall_workers.push(worker.clone());
            match &decision {
                ShortfallDecision::Grant { program } => {
                    c.granted_programs.push(program.clone());
                    c.grants.push(crate::shortfall::GrantRecord {
                        program: program.clone(),
                        worker: worker.to_string(),
                        why: why.clone(),
                        at_ms: now,
                    });
                    c.shortfall_redispatches += 1;
                }
                ShortfallDecision::Amend => {
                    c.shortfall_notes.push(format!("{need}: {why}"));
                    c.shortfall_redispatches += 1;
                }
                ShortfallDecision::Hold { reason } => {
                    c.shortfall_hold = Some(kgraf_secrets::scrub_persist_owned(reason.clone()));
                }
            }
        }
        let resolution = decision.describe();
        self.trace(TraceEvent::Shortfall {
            at_ms: now,
            contract: contract_id.clone(),
            worker: worker.to_string(),
            kind: kind.as_str().to_owned(),
            need: need.clone(),
            why: why.clone(),
            resolution: resolution.clone(),
        })
        .await?;
        self.submit_report(
            &contract,
            Some(&worker),
            ReportKind::NeedsInput,
            &format!("{} shortfall: {need}: {resolution}", kind.as_str()),
        );
        self.save()?;
        // The answer goes first; the cancel reaches the worker after its
        // tool call returns, so the transcript shows the resolution.
        if let Some(flight) = self.in_flight.get(&contract_id)
            && let Some(n) = flight.cancels.get(&worker)
        {
            n.notify_one();
        }
        Ok(ContractReply::Shortfall(resolution))
    }

    /// Whether `worker`'s attempt ended in a shortfall report. If it did, the
    /// attempt is marked stopped and the flight renews its workers: the next
    /// attempt is a fresh worker, whose environment carries any grant.
    pub(super) fn take_shortfall(
        &mut self,
        contract: &str,
        worker: &AgentId,
        flight: &mut InFlight,
    ) -> bool {
        let Some(c) = self.record.contract_mut(contract) else {
            return false;
        };
        let Some(at) = c.shortfall_workers.iter().position(|w| w == worker) else {
            return false;
        };
        c.shortfall_workers.remove(at);
        if let Some(a) = flight.attempts.iter_mut().find(|a| a.worker == *worker) {
            a.status = AttemptStatus::Stopped;
        }
        flight.renew_workers = true;
        true
    }
}
