//! An in-process [`MessagePort`] for a channel that lives in the same process as
//! the orchestrator: the desktop shell's orchestrator box.
//!
//! It is the same port the CLI's `--interactive` stdin channel implements, with the
//! same two message kinds (a note, a status question) and the same replies. Only the
//! transport differs: two queues instead of a terminal. No command kind exists here
//! either; inbound text stays data ([`UntrustedText`]).
//!
//! The two halves:
//! - [`ChannelPort`] is what the orchestrator is given ([`crate::Orchestrator::attach_port`]).
//! - [`PortClient`] is what the UI holds. It is synchronous, because the UI thread
//!   is not an async task: sending never waits, and replies are drained without
//!   blocking. Every reply calls the `wake` function the client was built with, so
//!   an idle UI that repaints only on change still sees it.
//!
//! Dropping the client closes the channel: the orchestrator's receive returns
//! `None` and its port task ends. A reply to a dropped client is
//! [`PortError::Closed`], which the orchestrator records rather than assuming
//! delivery.
//!
//! Written new: the port trait is this crate's own, and the transport is two
//! standard channels. Searched: the CLI's `StdinPort` (the only other
//! implementation) is bound to stdin; tokio's `mpsc` is the transport here.

use crate::port::{InboundKind, InboundMessage, MessagePort, OutboundReply, PortError};
use kgraf_tools::UntrustedText;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Messages the orchestrator has not yet taken. A person types into the box, so
/// this is never reached in use; when it is, the send is refused, never dropped.
pub const PORT_QUEUE: usize = 64;

/// Called after every reply so the UI repaints.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// The orchestrator's half.
pub struct ChannelPort {
    channel: String,
    inbound: Mutex<mpsc::Receiver<InboundMessage>>,
    replies: std::sync::mpsc::Sender<OutboundReply>,
    wake: Wake,
}

impl std::fmt::Debug for ChannelPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelPort")
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// The UI's half.
#[derive(Debug)]
pub struct PortClient {
    channel: String,
    outbound: mpsc::Sender<InboundMessage>,
    replies: std::sync::mpsc::Receiver<OutboundReply>,
    sent: u64,
}

/// A channel port pair named `channel`. `wake` runs after each reply.
pub fn channel_port(channel: &str, wake: Wake) -> (Arc<ChannelPort>, PortClient) {
    let (tx, rx) = mpsc::channel(PORT_QUEUE);
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    (
        Arc::new(ChannelPort {
            channel: channel.to_owned(),
            inbound: Mutex::new(rx),
            replies: reply_tx,
            wake,
        }),
        PortClient {
            channel: channel.to_owned(),
            outbound: tx,
            replies: reply_rx,
            sent: 0,
        },
    )
}

impl PortClient {
    /// The channel's name.
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Queue one message. Returns its id, which the reply will name. A full queue
    /// or a closed orchestrator is an error: the message was not delivered.
    pub fn send(
        &mut self,
        kind: InboundKind,
        text: &str,
        now_ms: i64,
    ) -> Result<String, PortError> {
        self.sent += 1;
        let id = format!("{}-{}-{now_ms}", self.channel, self.sent);
        let message = InboundMessage {
            id: id.clone(),
            channel: self.channel.clone(),
            received_ms: now_ms,
            kind,
            text: UntrustedText::new(text),
        };
        match self.outbound.try_send(message) {
            Ok(()) => Ok(id),
            Err(mpsc::error::TrySendError::Full(_)) => Err(PortError::Transport {
                channel: self.channel.clone(),
                reason: format!("{PORT_QUEUE} messages are waiting for the orchestrator"),
            }),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(PortError::Closed {
                channel: self.channel.clone(),
            }),
        }
    }

    /// Every reply that has arrived, without waiting.
    pub fn drain_replies(&self) -> Vec<OutboundReply> {
        self.replies.try_iter().collect()
    }
}

#[async_trait::async_trait]
impl MessagePort for ChannelPort {
    fn channel(&self) -> &str {
        &self.channel
    }

    async fn receive(&self) -> Result<Option<InboundMessage>, PortError> {
        Ok(self.inbound.lock().await.recv().await)
    }

    async fn reply(&self, reply: OutboundReply) -> Result<(), PortError> {
        self.replies.send(reply).map_err(|_| PortError::Closed {
            channel: self.channel.clone(),
        })?;
        (self.wake)();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counter() -> (Wake, Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        (
            Arc::new(move || {
                m.fetch_add(1, Ordering::SeqCst);
            }),
            n,
        )
    }

    #[tokio::test]
    async fn a_message_crosses_as_data_and_its_reply_wakes_the_ui() {
        let (wake, woken) = counter();
        let (port, mut client) = channel_port("box", wake);
        let id = client
            .send(InboundKind::StatusQuestion, "how far?", 7)
            .expect("send");
        let m = port.receive().await.expect("ok").expect("message");
        assert_eq!(m.id, id);
        assert_eq!(m.kind, InboundKind::StatusQuestion);
        assert_eq!(m.channel, "box");
        assert_eq!(m.received_ms, 7);
        assert!(m.to_data_block().contains("\nhow far?\n"));
        port.reply(OutboundReply {
            in_reply_to: id.clone(),
            text: "2 of 3 verified".into(),
        })
        .await
        .expect("reply");
        assert_eq!(woken.load(Ordering::SeqCst), 1);
        let replies = client.drain_replies();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].in_reply_to, id);
        assert!(client.drain_replies().is_empty(), "drained once");
    }

    #[tokio::test]
    async fn ids_are_unique_per_message() {
        let (wake, _) = counter();
        let (_port, mut client) = channel_port("box", wake);
        let a = client.send(InboundKind::Note, "a", 1).expect("a");
        let b = client.send(InboundKind::Note, "b", 1).expect("b");
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn dropping_the_client_closes_the_channel_both_ways() {
        let (wake, _) = counter();
        let (port, client) = channel_port("box", wake);
        drop(client);
        assert_eq!(port.receive().await.expect("ok"), None);
        let err = port
            .reply(OutboundReply {
                in_reply_to: "x".into(),
                text: "late".into(),
            })
            .await
            .expect_err("closed");
        assert!(matches!(err, PortError::Closed { .. }));
    }

    #[tokio::test]
    async fn a_full_queue_refuses_the_send_instead_of_dropping_it() {
        let (wake, _) = counter();
        let (port, mut client) = channel_port("box", wake);
        for i in 0..PORT_QUEUE {
            client
                .send(InboundKind::Note, &format!("n{i}"), 0)
                .expect("fits");
        }
        let err = client
            .send(InboundKind::Note, "one too many", 0)
            .expect_err("full");
        assert!(matches!(err, PortError::Transport { .. }), "{err}");
        drop(port);
        let err = client
            .send(InboundKind::Note, "gone", 0)
            .expect_err("closed");
        assert!(matches!(err, PortError::Closed { .. }), "{err}");
    }
}
