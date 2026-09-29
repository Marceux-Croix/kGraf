//! The steering seam (decision 0067, point 3): [`Injector::inject`] is how a
//! message reaches a running worker's transcript, at the next tool-batch
//! boundary or, in interrupt mode, at once.
//!
//! **The queue.** Each worker has one [`SteerQueue`], shared by its
//! [`Injector`]s and its loop. `inject` appends to it; the loop drains it
//! whole, first in, first out, at the boundary between rounds (after the
//! step-budget notice, before the request is built) and turns each item into a
//! [`crate::WorkerEvent::Injected`]. Nothing is ever read out of the queue
//! except by that drain, so an accepted item either reaches the transcript or
//! is still listed by [`Injector::undelivered`] when the worker ends.
//!
//! **Interrupt mode** arms a level-triggered flag. The loop races it against
//! the model stream and against running tools; the drain disarms it. A flag
//! armed while nothing is interruptible (between rounds, during an approval
//! ask) is simply seen at the next interruptible phase or cleared by the next
//! drain.
//!
//! **Delivery guarantee.** Exactly once **in the transcript**;
//! at least once **to the provider**. The loop records `Injected` before it
//! sends the request that carries the note. If the process dies after that
//! request left and before the transcript writer appended the event, the
//! resumed worker delivers the note again in its first request, so the
//! provider has then seen it twice, once in a request whose answer was lost.
//! The resumed conversation, which is what the model continues from, holds it
//! once. A crash after the append finds the id in the transcript and refuses a
//! second copy.
//!
//! **Refusals.** A worker that has finished closes its queue, so `inject`
//! returns [`InjectError::Finished`] at once and never blocks (the caller then
//! offers the text to the orchestrator). An id already queued, or already in
//! the transcript (a resume seeds those, see [`crate::ResumeState::injected_ids`]),
//! returns [`InjectError::Duplicate`]: the transcript is the truth for
//! "delivered", so a crash between the transcript append and the inbox update
//! delivers nothing twice.
//!
//! **Ported.** The queue and its drain rules are a port of LibreChat's
//! steering queue: `packages/api/src/stream/implementations/InMemoryJobStore.ts`
//! (`enqueueSteerVersioned`, `drainSteers`, `closeAndDrainSteers`,
//! `restoreClaimedSteers`) and `packages/api/src/agents/steering/runtime.ts`
//! (`drainAndBuildInjections`, `createSteerPreemptPoll`), commit
//! 7b2362d7a7c6148b84850924dc7fa5fc43307923, MIT; see
//! `third_party/librechat/UPSTREAM`. Kept from upstream: FIFO order is the
//! instruction order; enqueue is refused once the run is no longer running;
//! the drain takes the whole queue; a failed apply restores the failed item
//! and everything after it at the queue front, so no later item overtakes it;
//! the preempt flag is level-triggered and cleared by the drain, never on
//! read; the queue is closed when the run ends and its leftovers are reported,
//! not dropped. Changed: one process owns the queue, so there are no receipts,
//! replica fences or claim expiry; the idempotency key is the inbox id (the
//! durable row is `kgraf-core::session::InboxItem`, written by the
//! orchestrator before it acknowledges); and there is no depth cap
//! (`STEER_QUEUE_MAX_DEPTH`), because decision 0067 never refuses or drops a
//! message for volume; the soft limits of `ParallelismLimits` slow a sender
//! instead. Not ported: upstream's terminal continuation (`StopFinalize`),
//! which reopens a finished turn to deliver a late steer. Here a worker that
//! answered has retired (one worker serves one work item), so a late item is
//! a leftover the orchestrator routes, not a reason to run another round.

use kgraf_core::AgentId;
use kgraf_core::session::{InboxKind, InboxSource, InjectMode};
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

/// One piece of text to inject into a running worker's transcript.
#[derive(Clone, Debug)]
pub struct Injection {
    pub id: String,
    pub source: InboxSource,
    pub kind: InboxKind,
    pub mode: InjectMode,
    pub text: String,
}

/// A handle to inject into one worker. The orchestrator keeps one per running
/// attempt (next to its cancel token, in `InFlight`). Cloning shares the same
/// worker and the same queue.
#[derive(Clone)]
pub struct Injector {
    worker: AgentId,
    queue: Arc<SteerQueue>,
}

impl std::fmt::Debug for Injector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injector")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl Injector {
    pub(crate) fn new(worker: AgentId, queue: Arc<SteerQueue>) -> Self {
        Self { worker, queue }
    }

    /// The worker this injector reaches.
    pub fn worker(&self) -> &AgentId {
        &self.worker
    }

    /// Queue `injection` for this worker. Returns at once: `Ok` means queued,
    /// not delivered; delivery is the `Injected` event on the worker's event
    /// stream. Refused with [`InjectError::Finished`] once the worker has
    /// ended, and with [`InjectError::Duplicate`] for an id this worker has
    /// already queued or delivered.
    pub fn inject(&self, injection: Injection) -> Result<(), InjectError> {
        self.queue.push(&self.worker, injection)
    }

    /// Ids accepted by [`Self::inject`] (or passed in `SpawnOptions::injections`)
    /// and not yet delivered, in queue order. After the worker ends, these are
    /// the items it never reached a boundary for: the caller routes them (to
    /// the orchestrator), because they are not in the transcript.
    pub fn undelivered(&self) -> Vec<String> {
        self.queue
            .lock()
            .items
            .iter()
            .map(|i| i.id.clone())
            .collect()
    }

    /// Whether the worker has ended, so every further `inject` is refused.
    pub fn is_finished(&self) -> bool {
        self.queue.lock().closed
    }
}

/// Why an injection was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InjectError {
    /// Kept from an earlier seam so callers written against it still compile. The
    /// runtime no longer returns it.
    #[error("steering is not built yet (decision 0067)")]
    NotBuilt,
    /// The worker has already finished; nothing is running to inject into.
    #[error("worker `{worker}` has already finished")]
    Finished { worker: String },
    /// The same injection id was already queued or delivered (exactly-once
    /// across a crash; see the module docs).
    #[error("injection `{id}` was already delivered")]
    Duplicate { id: String },
}

#[derive(Default)]
struct QueueState {
    /// Accepted and not yet drained, in arrival order.
    items: VecDeque<Injection>,
    /// Every id queued or delivered in this worker's life, plus the ids a
    /// resume found in the transcript.
    known: BTreeSet<String>,
    /// Set when the worker ends; every later push is refused.
    closed: bool,
}

/// One worker's steering queue (see the module docs).
pub(crate) struct SteerQueue {
    state: Mutex<QueueState>,
    /// The interrupt request: level-triggered, set by an interrupt-mode push,
    /// cleared by the drain. A `watch` so a waiter that subscribes after the
    /// flag was set still sees it (upstream's `shouldPreempt` poll plus its
    /// `subscribe` wake, in one primitive).
    armed: watch::Sender<bool>,
}

impl SteerQueue {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState::default()),
            armed: watch::Sender::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        // A poisoned lock means a panic while holding it; the state itself is
        // a queue and a set, each left consistent by every method below.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, worker: &AgentId, injection: Injection) -> Result<(), InjectError> {
        let mut state = self.lock();
        if state.closed {
            return Err(InjectError::Finished {
                worker: worker.to_string(),
            });
        }
        if !state.known.insert(injection.id.clone()) {
            return Err(InjectError::Duplicate { id: injection.id });
        }
        let interrupt = injection.mode == InjectMode::Interrupt;
        state.items.push_back(injection);
        // Armed under the lock, so a drain cannot run between the push and
        // the arm and leave the flag set with nothing queued.
        if interrupt {
            self.armed.send_replace(true);
        }
        Ok(())
    }

    /// Record ids the transcript already holds, so a second copy is refused.
    pub(crate) fn seed_delivered(&self, ids: impl IntoIterator<Item = String>) {
        self.lock().known.extend(ids);
    }

    /// Queue an item at spawn (`SpawnOptions::injections`). Never arms the
    /// interrupt: nothing is running yet, and the first boundary comes first.
    pub(crate) fn push_at_spawn(
        &self,
        worker: &AgentId,
        mut injection: Injection,
    ) -> Result<(), InjectError> {
        injection.mode = InjectMode::AtBoundary;
        self.push(worker, injection)
    }

    /// The estimated tokens of every queued item, without taking them: the
    /// loop's context and spend checks count what the next drain will add
    /// before deciding to run the round.
    pub(crate) fn queued_tokens(&self) -> u64 {
        self.lock()
            .items
            .iter()
            .map(|i| kgraf_core::budget::estimate_tokens(&i.text))
            .sum()
    }

    /// Drain every queued item, in order, through `apply`. If `apply` fails
    /// on an item, that item and every item after it go back to the queue
    /// front (upstream's `restoreClaimed`: nothing overtakes a failed item)
    /// and the error is returned. `apply` must not touch this queue.
    pub(crate) fn deliver<E>(
        &self,
        mut apply: impl FnMut(&Injection) -> Result<(), E>,
    ) -> Result<(), E> {
        let items = self.drain();
        for (i, item) in items.iter().enumerate() {
            if let Err(e) = apply(item) {
                self.restore_front(items[i..].to_vec());
                return Err(e);
            }
        }
        Ok(())
    }

    /// Take every queued item, in order, and disarm the interrupt.
    pub(crate) fn drain(&self) -> Vec<Injection> {
        let mut state = self.lock();
        self.armed.send_replace(false);
        state.items.drain(..).collect()
    }

    /// Put back items a drain took and did not deliver, ahead of anything
    /// queued since, in their original order (upstream's `restoreClaimed`).
    pub(crate) fn restore_front(&self, items: Vec<Injection>) {
        let mut state = self.lock();
        for item in items.into_iter().rev() {
            state.items.push_front(item);
        }
    }

    /// Refuse every later push. Queued items stay listed as undelivered.
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        self.armed.send_replace(false);
    }

    /// Resolves once an interrupt is armed (at once if it already is).
    pub(crate) fn interrupt_armed(&self) -> impl std::future::Future<Output = ()> + use<> {
        let mut rx = self.armed.subscribe();
        async move {
            // The sender lives as long as the queue, and the loop holds the
            // queue while it waits, so `Err` (sender gone) cannot happen; if it
            // did, no interrupt could ever arrive.
            if rx.wait_for(|armed| *armed).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Whether an interrupt is armed now.
    pub(crate) fn is_armed(&self) -> bool {
        *self.armed.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn injection(id: &str, mode: InjectMode) -> Injection {
        Injection {
            id: id.into(),
            source: InboxSource::User,
            kind: InboxKind::Note,
            mode,
            text: format!("text of {id}"),
        }
    }

    fn injector() -> Injector {
        Injector::new(AgentId::new("w1"), SteerQueue::new())
    }

    #[test]
    fn items_drain_first_in_first_out_and_whole() {
        let injector = injector();
        for id in ["a", "b", "c"] {
            injector
                .inject(injection(id, InjectMode::AtBoundary))
                .expect("queued");
        }
        assert_eq!(injector.undelivered(), ["a", "b", "c"]);
        let drained: Vec<String> = injector.queue.drain().into_iter().map(|i| i.id).collect();
        assert_eq!(drained, ["a", "b", "c"]);
        assert!(injector.undelivered().is_empty());
    }

    #[test]
    fn an_id_queued_or_delivered_before_is_a_duplicate() {
        let injector = injector();
        injector
            .inject(injection("a", InjectMode::AtBoundary))
            .expect("queued");
        assert_eq!(
            injector.inject(injection("a", InjectMode::AtBoundary)),
            Err(InjectError::Duplicate { id: "a".into() })
        );
        let _ = injector.queue.drain();
        assert_eq!(
            injector.inject(injection("a", InjectMode::AtBoundary)),
            Err(InjectError::Duplicate { id: "a".into() }),
            "a delivered id stays known after the drain"
        );
        injector
            .queue
            .seed_delivered(["from-transcript".to_owned()]);
        assert!(matches!(
            injector.inject(injection("from-transcript", InjectMode::AtBoundary)),
            Err(InjectError::Duplicate { .. })
        ));
    }

    #[test]
    fn a_closed_queue_refuses_with_finished_and_keeps_its_leftovers() {
        let injector = injector();
        injector
            .inject(injection("left", InjectMode::AtBoundary))
            .expect("queued");
        injector.queue.close();
        assert!(injector.is_finished());
        assert_eq!(
            injector.inject(injection("late", InjectMode::AtBoundary)),
            Err(InjectError::Finished {
                worker: "w1".into()
            })
        );
        assert_eq!(injector.undelivered(), ["left"]);
    }

    #[test]
    fn restored_items_go_back_in_front_in_their_order() {
        let injector = injector();
        for id in ["a", "b"] {
            injector
                .inject(injection(id, InjectMode::AtBoundary))
                .expect("queued");
        }
        let drained = injector.queue.drain();
        injector
            .inject(injection("c", InjectMode::AtBoundary))
            .expect("queued");
        injector.queue.restore_front(drained);
        assert_eq!(injector.undelivered(), ["a", "b", "c"]);
    }

    #[test]
    fn a_failed_delivery_puts_the_item_and_its_suffix_back_in_front() {
        let injector = injector();
        for id in ["a", "b", "c"] {
            injector
                .inject(injection(id, InjectMode::AtBoundary))
                .expect("queued");
        }
        let mut applied = Vec::new();
        let outcome = injector.queue.deliver(|item| {
            if item.id == "b" {
                return Err("refused");
            }
            applied.push(item.id.clone());
            Ok(())
        });
        assert_eq!(outcome, Err("refused"));
        assert_eq!(applied, ["a"]);
        assert_eq!(
            injector.undelivered(),
            ["b", "c"],
            "the failed item and everything after it are still undelivered, in order"
        );
        let mut again = Vec::new();
        injector
            .queue
            .deliver(|item| {
                again.push(item.id.clone());
                Ok::<(), ()>(())
            })
            .expect("delivered");
        assert_eq!(again, ["b", "c"]);
    }

    #[test]
    fn queued_tokens_count_the_queue_without_draining_it() {
        let injector = injector();
        assert_eq!(injector.queue.queued_tokens(), 0);
        injector
            .inject(injection("a", InjectMode::AtBoundary))
            .expect("queued");
        assert!(injector.queue.queued_tokens() > 0);
        assert_eq!(injector.undelivered(), ["a"]);
    }

    #[test]
    fn only_an_interrupt_arms_and_the_drain_disarms() {
        let injector = injector();
        injector
            .inject(injection("a", InjectMode::AtBoundary))
            .expect("queued");
        assert!(!injector.queue.is_armed());
        injector
            .inject(injection("b", InjectMode::Interrupt))
            .expect("queued");
        assert!(injector.queue.is_armed());
        let _ = injector.queue.drain();
        assert!(!injector.queue.is_armed());
        injector
            .queue
            .push_at_spawn(&AgentId::new("w1"), injection("c", InjectMode::Interrupt))
            .expect("queued");
        assert!(
            !injector.queue.is_armed(),
            "an item queued at spawn waits for the first boundary"
        );
    }

    #[tokio::test]
    async fn a_waiter_sees_an_interrupt_armed_before_or_after_it_subscribed() {
        let injector = injector();
        injector
            .inject(injection("a", InjectMode::Interrupt))
            .expect("queued");
        tokio::time::timeout(Duration::from_secs(1), injector.queue.interrupt_armed())
            .await
            .expect("already armed resolves at once");

        let _ = injector.queue.drain();
        let waiting = injector.queue.interrupt_armed();
        let later = injector.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            later
                .inject(injection("b", InjectMode::Interrupt))
                .expect("queued");
        });
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("an arm after subscribing wakes the waiter");
    }

    #[test]
    fn cloning_shares_the_same_worker_and_queue() {
        let injector = injector();
        let clone = injector.clone();
        assert_eq!(injector.worker(), clone.worker());
        clone
            .inject(injection("a", InjectMode::AtBoundary))
            .expect("queued");
        assert_eq!(injector.undelivered(), ["a"]);
    }
}
