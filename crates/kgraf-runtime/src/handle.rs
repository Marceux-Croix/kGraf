//! The two ends of a running worker.
//!
//! [`worker_channel`] makes a pair: the [`WorkerHandle`] the orchestrator holds, and
//! the [`WorkerEndpoint`] the loop holds. They share one cancellation token, one
//! ordered event channel and a one-shot result.
//!
//! **Two classes of event, one order.** Events travel on one channel, so the
//! transcript order is the emit order. Backpressure applies only to **bulk** events
//! (text and reasoning deltas, tool results; [`WorkerEvent::is_bulk`]): each carries
//! a permit from a semaphore of `bulk_capacity`, released when the reader takes the
//! event. A slow reader therefore slows its own worker instead of growing memory.
//! **Control** events (usage, stop, tool start, approval, cancelled) take no permit
//! and never wait. Their number is bounded by the number of provider and tool calls
//! in the run, which the step budget bounds; that is the memory the reader can be
//! behind by when it stops reading.
//!
//! That split is what makes a cancelled call accountable (every token is attributed to the ledger): the partial
//! `Usage` and the `Cancelled` event are emitted after the token is tripped, and
//! neither can be refused by the cancel nor stuck behind a full channel.
//!
//! **Mid-stream cancel.** [`WorkerHandle::cancel`] trips the token at once. A bulk
//! emit races its permit against the token and against the reader going away, so a
//! loop blocked on a stalled reader is still cancellable. The provider stream itself
//! is reached through [`crate::stream_bridge`].
//!
//! **A vanished worker is an error.** If the endpoint is dropped without
//! [`WorkerEndpoint::finish`], [`WorkerHandle::result`] returns
//! [`RuntimeError::WorkerVanished`], never a default result (fail loud).
//!
//! **Approval decisions go through the endpoint.** `WorkerEndpoint` implements
//! `ApprovalLog`, so the approval chain's records become
//! [`WorkerEvent::Approval`] events in the transcript.

use crate::brief::WorkerResult;
use crate::event::WorkerEvent;
use crate::inject::{Injector, SteerQueue};
use kgraf_core::AgentId;
use kgraf_tools::ApprovalError;
use kgraf_tools::approval::{ApprovalLog, ApprovalRecord};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Runtime failures at the handle and bridge boundaries.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The worker was cancelled.
    #[error("worker `{worker}` was cancelled")]
    Cancelled { worker: String },
    /// The handle is gone, so nobody can read the event.
    #[error("worker `{worker}` has no reader")]
    HandleDropped { worker: String },
    /// The worker ended without delivering a result.
    #[error("worker `{worker}` ended without a result")]
    WorkerVanished { worker: String },
    /// The provider stream produced more unread output than the bridge holds.
    #[error(
        "stream output exceeded the {limit_bytes}-byte bridge buffer; the run is failed, not truncated"
    )]
    StreamOverflow { limit_bytes: usize },
}

struct Envelope {
    event: WorkerEvent,
    /// Held while the event is queued; dropping it frees a bulk slot.
    _permit: Option<OwnedSemaphorePermit>,
}

/// Make a connected handle and endpoint. `bulk_capacity` bounds queued bulk events;
/// zero is raised to one.
pub fn worker_channel(worker: AgentId, bulk_capacity: usize) -> (WorkerHandle, WorkerEndpoint) {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (result_tx, result_rx) = oneshot::channel();
    let cancel = CancellationToken::new();
    let steering = SteerQueue::new();
    (
        WorkerHandle {
            worker: worker.clone(),
            events: event_rx,
            result: result_rx,
            cancel: cancel.clone(),
            steering: steering.clone(),
            already_delivered: Vec::new(),
            listed_twice: Vec::new(),
            skills: None,
        },
        WorkerEndpoint {
            worker,
            events: event_tx,
            bulk: Arc::new(Semaphore::new(bulk_capacity.max(1))),
            result: Some(result_tx),
            cancel,
            steering,
        },
    )
}

/// The orchestrator's end.
pub struct WorkerHandle {
    worker: AgentId,
    events: mpsc::UnboundedReceiver<Envelope>,
    result: oneshot::Receiver<WorkerResult>,
    cancel: CancellationToken,
    /// The worker's steering queue (decision 0067), shared with the loop.
    steering: Arc<SteerQueue>,
    /// Ids from `SpawnOptions::injections` the transcript already holds.
    already_delivered: Vec<String>,
    /// Ids `SpawnOptions::injections` named more than once (extra copies).
    listed_twice: Vec<String>,
    /// The worker's `allowed-tools` policy (decision 0053), when it has one.
    skills: Option<kgraf_session::skills::WorkerSkills>,
}

impl std::fmt::Debug for WorkerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerHandle")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl WorkerHandle {
    /// The worker's id.
    pub fn worker(&self) -> &AgentId {
        &self.worker
    }

    /// An [`Injector`] reaching this worker (decision 0067, point 3). Every
    /// injector of one handle shares the worker's one queue; the orchestrator
    /// keeps one next to this handle's cancel token. It stays usable after the
    /// handle is consumed: it then refuses with `Finished` and still lists the
    /// items the worker never delivered.
    pub fn injector(&self) -> Injector {
        Injector::new(self.worker.clone(), self.steering.clone())
    }

    /// Ids passed in `SpawnOptions::injections` that were not queued because
    /// the resumed transcript already carries an `Injected` event with that id.
    /// Each is delivered, exactly once in the transcript, so the caller marks
    /// its inbox row delivered rather than sending it again (decision 0067,
    /// "exactly once across a crash").
    pub fn already_delivered(&self) -> &[String] {
        &self.already_delivered
    }

    /// Ids that `SpawnOptions::injections` named more than once. The first
    /// copy is queued as usual; these are the extra copies, which were not.
    /// A caller defect, reported rather than hidden: the inbox should hold one
    /// row per id.
    pub fn listed_twice(&self) -> &[String] {
        &self.listed_twice
    }

    pub(crate) fn set_spawn_duplicates(
        &mut self,
        already_delivered: Vec<String>,
        listed_twice: Vec<String>,
    ) {
        self.already_delivered = already_delivered;
        self.listed_twice = listed_twice;
    }

    pub(crate) fn set_skills(&mut self, skills: Option<kgraf_session::skills::WorkerSkills>) {
        self.skills = skills;
    }

    /// The worker's skill restrictions (decision 0053), live: activations the
    /// worker makes later show in it. What a subagent it spawns inherits.
    /// `None`: the worker has no skill policy, so nothing restricts it.
    pub fn skills(&self) -> Option<kgraf_session::skills::WorkerSkills> {
        self.skills.clone()
    }

    /// Cancel the worker. Returns at once; the worker emits `Cancelled`, settles
    /// its open call with any partial usage, and delivers a `Cancelled` result.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Whether cancel has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The next event, or `None` once the worker has finished emitting.
    pub async fn next_event(&mut self) -> Option<WorkerEvent> {
        self.events.recv().await.map(|envelope| envelope.event)
    }

    /// Wait for the result. Events not yet read are discarded; read them first when
    /// they matter.
    pub async fn result(self) -> Result<WorkerResult, RuntimeError> {
        let worker = self.worker.to_string();
        // Dropping the receiver closes the channel, which unblocks a loop waiting
        // for a bulk slot.
        drop(self.events);
        self.result
            .await
            .map_err(|_| RuntimeError::WorkerVanished { worker })
    }
}

/// The loop's end.
pub struct WorkerEndpoint {
    worker: AgentId,
    events: mpsc::UnboundedSender<Envelope>,
    bulk: Arc<Semaphore>,
    result: Option<oneshot::Sender<WorkerResult>>,
    cancel: CancellationToken,
    steering: Arc<SteerQueue>,
}

impl std::fmt::Debug for WorkerEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerEndpoint")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl WorkerEndpoint {
    /// The worker's id.
    pub fn worker(&self) -> &AgentId {
        &self.worker
    }

    /// Send one event.
    ///
    /// A bulk event waits for a slot, and gives up with `Cancelled` if the worker is
    /// cancelled first or `HandleDropped` if the reader goes away. A control event
    /// never waits and is sent even after cancel.
    pub async fn emit(&self, event: WorkerEvent) -> Result<(), RuntimeError> {
        let permit = if event.is_bulk() {
            let acquire = self.bulk.clone().acquire_owned();
            let permit = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    return Err(RuntimeError::Cancelled { worker: self.worker.to_string() });
                }
                () = self.events.closed() => {
                    return Err(self.dropped());
                }
                permit = acquire => permit.map_err(|_| self.dropped())?,
            };
            Some(permit)
        } else {
            None
        };
        self.events
            .send(Envelope {
                event,
                _permit: permit,
            })
            .map_err(|_| self.dropped())
    }

    /// Send an event without waiting for a bulk slot, even after a cancel.
    ///
    /// For the results of tool calls that finished inside the cancel grace period:
    /// what a call actually did must reach the transcript, and after a cancel a bulk
    /// emit is refused. Bounded by the number of calls in one round.
    pub(crate) fn emit_unbounded(&self, event: WorkerEvent) -> Result<(), RuntimeError> {
        self.events
            .send(Envelope {
                event,
                _permit: None,
            })
            .map_err(|_| self.dropped())
    }

    fn dropped(&self) -> RuntimeError {
        RuntimeError::HandleDropped {
            worker: self.worker.to_string(),
        }
    }

    /// Resolves when the worker is cancelled. Race stream reads and tool calls
    /// against it.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Whether cancel has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// A clone of the token, for a `ToolContext`.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The worker's steering queue, for the loop.
    pub(crate) fn steering(&self) -> Arc<SteerQueue> {
        self.steering.clone()
    }

    /// Deliver the result and close the event channel.
    ///
    /// Returns the result back when the handle is gone, so the caller can still
    /// record it. Boxed because the result is large and the error path is rare.
    pub fn finish(mut self, result: WorkerResult) -> Result<(), Box<WorkerResult>> {
        match self.result.take() {
            Some(tx) => tx.send(result).map_err(Box::new),
            None => Err(Box::new(result)),
        }
    }
}

#[async_trait::async_trait]
impl ApprovalLog for WorkerEndpoint {
    async fn record(&mut self, record: ApprovalRecord) -> Result<(), ApprovalError> {
        // Persist-scrubbed like every other approval record (decision 0071):
        // this log is the endpoint itself, with no loop in front of it.
        self.emit(WorkerEvent::Approval { record }.scrubbed_for_persist())
            .await
            .map_err(|e| ApprovalError::Log(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brief::WorkerOutcome;
    use crate::event::CancelPhase;
    use kgraf_core::{CallId, TokenUsage};
    use kgraf_tools::approval::RecordedDecision;
    use std::time::Duration;

    fn result(outcome: WorkerOutcome) -> WorkerResult {
        WorkerResult::bare("t", "w", outcome)
    }

    fn delta(text: &str) -> WorkerEvent {
        WorkerEvent::TextDelta { text: text.into() }
    }

    fn usage() -> WorkerEvent {
        WorkerEvent::Usage {
            call: CallId::new("c1"),
            usage: TokenUsage {
                text_output: 7,
                ..TokenUsage::default()
            },
        }
    }

    #[tokio::test]
    async fn the_handle_gives_an_injector_reaching_the_same_worker() {
        let (handle, _endpoint) = worker_channel("w".into(), 8);
        assert_eq!(handle.injector().worker(), handle.worker());
    }

    #[tokio::test]
    async fn events_flow_and_the_result_arrives() {
        let (mut handle, endpoint) = worker_channel("w".into(), 8);
        endpoint.emit(delta("hi")).await.expect("emit");
        endpoint
            .finish(result(WorkerOutcome::Completed))
            .expect("finish");
        assert_eq!(handle.next_event().await, Some(delta("hi")));
        assert_eq!(handle.next_event().await, None);
        assert_eq!(
            handle.result().await.expect("result").outcome,
            WorkerOutcome::Completed
        );
    }

    #[tokio::test]
    async fn after_a_cancel_the_partial_usage_and_the_cancel_still_reach_a_stalled_reader() {
        // Every token is attributed: a full bulk channel, a reader that is not reading, and a cancel.
        // The usage and the cancel event must still go out, in order, at once.
        let (mut handle, endpoint) = worker_channel("w".into(), 1);
        endpoint
            .emit(delta("fills the only bulk slot"))
            .await
            .expect("first");
        handle.cancel();
        assert!(matches!(
            endpoint.emit(delta("refused")).await,
            Err(RuntimeError::Cancelled { .. })
        ));
        tokio::time::timeout(Duration::from_millis(200), async {
            endpoint.emit(usage()).await.expect("usage");
            endpoint
                .emit(WorkerEvent::Cancelled {
                    phase: CancelPhase::ModelStream,
                })
                .await
                .expect("cancelled");
        })
        .await
        .expect("control events must not block on a full channel");
        drop(endpoint);
        assert_eq!(
            handle.next_event().await,
            Some(delta("fills the only bulk slot"))
        );
        assert_eq!(handle.next_event().await, Some(usage()));
        assert!(matches!(
            handle.next_event().await,
            Some(WorkerEvent::Cancelled { .. })
        ));
    }

    #[tokio::test]
    async fn a_cancel_interrupts_a_simulated_stream_mid_response() {
        let (mut handle, endpoint) = worker_channel("w".into(), 8);
        let worker = tokio::spawn(async move {
            let mut sent = 0u32;
            loop {
                tokio::select! {
                    () = endpoint.cancelled() => {
                        endpoint.emit(usage()).await.expect("partial usage");
                        endpoint
                            .emit(WorkerEvent::Cancelled { phase: CancelPhase::ModelStream })
                            .await
                            .expect("cancel event");
                        let _ = endpoint.finish(result(WorkerOutcome::Cancelled));
                        return sent;
                    }
                    () = tokio::time::sleep(Duration::from_millis(5)) => {
                        endpoint.emit(delta("x")).await.expect("delta");
                        sent += 1;
                    }
                }
            }
        });
        for _ in 0..2 {
            assert!(matches!(
                handle.next_event().await,
                Some(WorkerEvent::TextDelta { .. })
            ));
        }
        handle.cancel();
        let mut tail = Vec::new();
        while let Some(event) = handle.next_event().await {
            tail.push(event);
        }
        let n = tail.len();
        assert_eq!(tail[n - 2], usage());
        assert!(matches!(tail[n - 1], WorkerEvent::Cancelled { .. }));
        assert_eq!(
            handle.result().await.expect("result").outcome,
            WorkerOutcome::Cancelled
        );
        assert!(worker.await.expect("join") >= 2);
    }

    #[tokio::test]
    async fn a_loop_blocked_on_a_full_channel_is_released_by_cancel_or_by_the_reader_leaving() {
        let (handle, endpoint) = worker_channel("w".into(), 1);
        endpoint.emit(delta("fills")).await.expect("first fits");
        let endpoint = Arc::new(endpoint);
        let blocked = {
            let endpoint = endpoint.clone();
            tokio::spawn(async move { endpoint.emit(delta("blocks")).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        handle.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("did not hang")
            .expect("join");
        assert!(matches!(outcome, Err(RuntimeError::Cancelled { .. })));

        let (handle, endpoint) = worker_channel("w".into(), 1);
        endpoint.emit(delta("fills")).await.expect("first fits");
        let endpoint = Arc::new(endpoint);
        let blocked = {
            let endpoint = endpoint.clone();
            tokio::spawn(async move { endpoint.emit(delta("blocks")).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(handle);
        let outcome = tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("did not hang")
            .expect("join");
        assert!(matches!(outcome, Err(RuntimeError::HandleDropped { .. })));
    }

    #[tokio::test]
    async fn reading_frees_bulk_slots() {
        let (mut handle, endpoint) = worker_channel("w".into(), 1);
        for i in 0..5 {
            endpoint.emit(delta(&i.to_string())).await.expect("emit");
            assert_eq!(handle.next_event().await, Some(delta(&i.to_string())));
        }
    }

    #[tokio::test]
    async fn approval_records_become_transcript_events() {
        let (mut handle, mut endpoint) = worker_channel("w".into(), 1);
        let record = ApprovalRecord {
            call_id: "c".into(),
            tool: "shell".into(),
            decided_by: "chain".into(),
            decision: RecordedDecision::AllowedNoHooks,
        };
        ApprovalLog::record(&mut endpoint, record.clone())
            .await
            .expect("record");
        assert_eq!(
            handle.next_event().await,
            Some(WorkerEvent::Approval { record })
        );
    }

    #[tokio::test]
    async fn a_worker_that_vanishes_without_a_result_is_an_error() {
        let (handle, endpoint) = worker_channel("w".into(), 1);
        drop(endpoint);
        assert!(matches!(
            handle.result().await,
            Err(RuntimeError::WorkerVanished { .. })
        ));
    }

    #[tokio::test]
    async fn emitting_to_a_dropped_handle_is_an_error_and_finish_returns_the_result() {
        let (handle, endpoint) = worker_channel("w".into(), 1);
        drop(handle);
        assert!(matches!(
            endpoint.emit(usage()).await,
            Err(RuntimeError::HandleDropped { .. })
        ));
        let returned = endpoint
            .finish(result(WorkerOutcome::Completed))
            .expect_err("no reader");
        assert_eq!(returned.outcome, WorkerOutcome::Completed);
    }
}
