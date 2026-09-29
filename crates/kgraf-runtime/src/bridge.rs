//! The bridge from a provider's synchronous stream sink to the async loop.
//!
//! `Streaming::stream` (kgraf-provider) delivers events to a synchronous
//! `FnMut(StreamEvent)` sink, called inside the adapter's own future. A synchronous
//! sink cannot await, and blocking it would block an executor thread, so
//! **backpressure does not reach the provider stream**. What the bridge does
//! instead, stated plainly:
//!
//! - [`StreamSink::push`] never blocks. Deltas are counted against a byte limit;
//!   the bytes are released as [`StreamPump::next`] hands them on.
//! - The pump forwards to `WorkerEndpoint::emit`, where the reader's backpressure
//!   applies, so a slow reader lets unread deltas accumulate here, up to the limit.
//! - Past the limit, the run **fails** with `RuntimeError::StreamOverflow` at the
//!   point the limit was crossed. Output is never silently truncated (fail loud). Later
//!   deltas of that failed stream are discarded; `Usage` and `Finished` still pass,
//!   so the call is settled with its usage (every token is attributed).
//! - Memory is therefore bounded by the limit plus the control events of one call.
//!
//! The loop runs the provider's `stream` future and the pump concurrently (for
//! example with `tokio::join!`), with `&mut |e| sink.push(e)` as the sink.

use crate::handle::RuntimeError;
use kgraf_provider::{Content, StreamEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

enum Item {
    Event { event: StreamEvent, bytes: usize },
    Overflow,
}

/// Make a connected sink and pump with a limit of `limit_bytes` of unread deltas.
pub fn stream_bridge(limit_bytes: usize) -> (StreamSink, StreamPump) {
    let (tx, rx) = mpsc::unbounded_channel();
    let buffered = Arc::new(AtomicUsize::new(0));
    (
        StreamSink {
            tx,
            buffered: buffered.clone(),
            limit_bytes,
            overflowed: false,
        },
        StreamPump {
            rx,
            buffered,
            limit_bytes,
        },
    )
}

/// The synchronous end, given to the provider.
pub struct StreamSink {
    tx: mpsc::UnboundedSender<Item>,
    buffered: Arc<AtomicUsize>,
    limit_bytes: usize,
    overflowed: bool,
}

impl StreamSink {
    /// Accept one event from the provider. Never blocks.
    ///
    /// A send to a pump that has gone away is ignored here: the loop that owns the
    /// pump has already failed or been cancelled, and it settles the call.
    pub fn push(&mut self, event: StreamEvent) {
        let Some(bytes) = delta_bytes(&event) else {
            let _ = self.tx.send(Item::Event { event, bytes: 0 });
            return;
        };
        if self.overflowed {
            return;
        }
        let held = self.buffered.load(Ordering::Acquire);
        if held.saturating_add(bytes) > self.limit_bytes {
            self.overflowed = true;
            let _ = self.tx.send(Item::Overflow);
            return;
        }
        self.buffered.fetch_add(bytes, Ordering::AcqRel);
        let _ = self.tx.send(Item::Event { event, bytes });
    }
}

/// The async end, drained by the loop.
pub struct StreamPump {
    rx: mpsc::UnboundedReceiver<Item>,
    buffered: Arc<AtomicUsize>,
    limit_bytes: usize,
}

impl StreamPump {
    /// The next event; `Ok(None)` when the stream is done. Returns
    /// `Err(StreamOverflow)` once, at the point the limit was crossed; the events
    /// after it (control only) follow on later calls.
    pub async fn next(&mut self) -> Result<Option<StreamEvent>, RuntimeError> {
        match self.rx.recv().await {
            None => Ok(None),
            Some(Item::Event { event, bytes }) => {
                self.buffered.fetch_sub(bytes, Ordering::AcqRel);
                Ok(Some(event))
            }
            Some(Item::Overflow) => Err(RuntimeError::StreamOverflow {
                limit_bytes: self.limit_bytes,
            }),
        }
    }
}

/// Bytes a delta holds; `None` for control events, which are never limited.
fn delta_bytes(event: &StreamEvent) -> Option<usize> {
    match event {
        StreamEvent::Delta { content } => Some(match content {
            Content::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            other => other.as_text().map_or(0, str::len),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::TokenUsage;
    use kgraf_provider::StopReason;

    fn text(n: usize) -> StreamEvent {
        StreamEvent::Delta {
            content: Content::text("x".repeat(n)),
        }
    }

    /// Shaped like `Streaming::stream`: an async fn feeding a synchronous sink.
    async fn provider_stream(sink: &mut (dyn FnMut(StreamEvent) + Send), deltas: usize) {
        for _ in 0..deltas {
            sink(text(10));
            tokio::task::yield_now().await;
        }
        sink(StreamEvent::Usage {
            usage: TokenUsage {
                text_output: deltas as u64,
                ..TokenUsage::default()
            },
        });
        sink(StreamEvent::Finished {
            stop_reason: StopReason::EndTurn,
        });
    }

    #[tokio::test]
    async fn a_synchronous_sink_feeds_the_async_pump_concurrently() {
        let (mut sink, mut pump) = stream_bridge(1_000);
        let produce = async move {
            provider_stream(&mut |e| sink.push(e), 50).await;
        };
        let consume = async {
            let mut deltas = 0;
            let mut finished = false;
            while let Some(event) = pump.next().await.expect("no overflow") {
                match event {
                    StreamEvent::Delta { .. } => deltas += 1,
                    StreamEvent::Finished { .. } => finished = true,
                    _ => {}
                }
            }
            (deltas, finished)
        };
        let ((), (deltas, finished)) = tokio::join!(produce, consume);
        assert_eq!(deltas, 50);
        assert!(finished);
    }

    #[tokio::test]
    async fn an_unread_stream_past_the_limit_fails_loudly_and_still_delivers_its_usage() {
        let (mut sink, mut pump) = stream_bridge(45);
        // Nobody pumps while the provider streams 10 deltas of 10 bytes.
        provider_stream(&mut |e| sink.push(e), 10).await;
        drop(sink);
        for _ in 0..4 {
            assert!(matches!(
                pump.next().await,
                Ok(Some(StreamEvent::Delta { .. }))
            ));
        }
        assert_eq!(
            pump.next().await,
            Err(RuntimeError::StreamOverflow { limit_bytes: 45 })
        );
        assert!(matches!(
            pump.next().await,
            Ok(Some(StreamEvent::Usage { .. }))
        ));
        assert!(matches!(
            pump.next().await,
            Ok(Some(StreamEvent::Finished { .. }))
        ));
        assert_eq!(pump.next().await, Ok(None));
    }

    #[tokio::test]
    async fn pumped_bytes_free_room_for_more() {
        let (mut sink, mut pump) = stream_bridge(10);
        for _ in 0..20 {
            sink.push(text(10));
            assert!(matches!(
                pump.next().await,
                Ok(Some(StreamEvent::Delta { .. }))
            ));
        }
    }
}
