//! Ending the processes an attempt's commands left running
//! ([`AttemptProcesses`]).
//!
//! A command may start a process that outlives it (`sh -c "server &"`). Left
//! alone, it outlives the attempt too: it holds its port against the next
//! attempt of the contract, keeps writing the tree, or serves a later
//! attempt the earlier attempt's files. So:
//!
//! - When the tree is the whole delivery (commands run in the host sandbox),
//!   an attempt's processes are ended as soon as its worker finishes, before
//!   its tree is verified.
//! - When the processes are part of the delivery (commands run in a task's
//!   own container, whose state is judged), the accepted attempt's processes
//!   stay; every other attempt's are ended when its round is finalized.
//!
//! Each ending that found something, and each that failed, is a notice and a
//! trace `notice` event naming the contract and the attempt.

use super::*;

impl RunLoop {
    /// Worker `worker` of `contract` finished: end its processes now when
    /// the tree is the whole delivery; otherwise keep them for
    /// [`Self::end_unaccepted_processes`].
    pub(super) async fn on_attempt_processes_finished(
        &mut self,
        contract: &str,
        worker: &AgentId,
        flight: &mut InFlight,
    ) -> Result<(), OrchestratorError> {
        let Some(slot) = flight.processes.get(worker) else {
            return Ok(());
        };
        let Some(processes) = slot.get().cloned() else {
            // No environment was built, or it starts nothing it can end.
            flight.processes.remove(worker);
            return Ok(());
        };
        if processes.delivered() {
            return Ok(());
        }
        flight.processes.remove(worker);
        let ended = processes.end().await;
        self.note_ended_processes(contract, worker, "its worker finished", ended)
            .await
    }

    /// The round of `contract` is being finalized: end the processes of every
    /// attempt kept for it except `keep` (the accepted attempt, or one whose
    /// work continues).
    pub(super) async fn end_unaccepted_processes(
        &mut self,
        contract: &str,
        flight: &mut InFlight,
        keep: &[AgentId],
    ) -> Result<(), OrchestratorError> {
        let ending: Vec<AgentId> = flight
            .processes
            .keys()
            .filter(|w| !keep.contains(w))
            .cloned()
            .collect();
        for worker in ending {
            let Some(slot) = flight.processes.remove(&worker) else {
                continue;
            };
            let Some(processes) = slot.get().cloned() else {
                continue;
            };
            let ended = processes.end().await;
            self.note_ended_processes(contract, &worker, "its attempt was not accepted", ended)
                .await?;
        }
        Ok(())
    }

    async fn note_ended_processes(
        &mut self,
        contract: &str,
        worker: &AgentId,
        when: &str,
        ended: Result<Vec<String>, String>,
    ) -> Result<(), OrchestratorError> {
        let detail = match ended {
            Ok(lines) if lines.is_empty() => return Ok(()),
            Ok(lines) => format!(
                "contract {contract}: attempt {worker} left processes running; they were ended \
                 when {when}: {}",
                lines.join("; ")
            ),
            Err(e) => format!(
                "contract {contract}: the processes attempt {worker} left running could not be \
                 ended when {when}: {e}"
            ),
        };
        let detail = kgraf_secrets::scrub_persist_owned(detail);
        self.notices.insert(detail.clone());
        let at = self.now();
        self.trace(TraceEvent::Notice {
            at_ms: at,
            contract: contract.to_owned(),
            worker: worker.to_string(),
            detail,
        })
        .await
    }
}
