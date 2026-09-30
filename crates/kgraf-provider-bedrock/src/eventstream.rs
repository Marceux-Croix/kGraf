//! Decoding of `application/vnd.amazon.eventstream`, the binary framing
//! ConverseStream answers in.
//!
//! Frame parsing, length checks and both CRC32 checks are `aws-smithy-eventstream`'s
//! (a maintained AWS crate); this module only buffers incoming chunks, bounds the
//! buffer, and lifts the three headers Bedrock uses into a small owned type.
//!
//! Each frame carries `:message-type` (`event`, `exception` or `error`),
//! `:event-type` or `:exception-type`, and a JSON payload that is the event body
//! itself. Unlike the older InvokeModel stream, the payload is not wrapped in a
//! base64 `bytes` envelope.

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::event_stream::{HeaderValue, Message};
use bytes::BytesMut;

use crate::ADAPTER;
use kgraf_provider::ProviderError;

/// Bytes the decoder may hold without completing a frame. The service's own limit
/// on one event is far below this; a peer that never finishes a frame raises here
/// instead of growing the buffer.
pub const MAX_BUFFERED_BYTES: usize = 24 * 1024 * 1024;

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A stream event: its `:event-type` and JSON payload.
    Event { name: String, payload: Vec<u8> },
    /// A modeled exception the service sent inside a 200 stream.
    Exception { kind: String, payload: Vec<u8> },
    /// An unmodeled error frame (`:message-type` is `error`).
    Error { code: String, message: String },
}

/// Incremental decoder: feed chunks as they arrive, take complete frames out.
pub struct EventStreamDecoder {
    inner: MessageFrameDecoder,
    buffer: BytesMut,
}

impl Default for EventStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStreamDecoder {
    pub fn new() -> Self {
        Self {
            inner: MessageFrameDecoder::new(),
            buffer: BytesMut::new(),
        }
    }

    /// Bytes buffered that have not yet completed a frame. A frame whose 12-byte
    /// prelude was already read no longer counts those bytes.
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Feed one chunk; returns every frame it completed, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, ProviderError> {
        let (frames, error) = self.push_partial(chunk);
        match error {
            Some(error) => Err(error),
            None => Ok(frames),
        }
    }

    /// Feed one chunk; returns every frame decoded before a failure, and the failure
    /// if there was one. A corrupt frame late in a chunk must not discard the frames
    /// before it: the last of them can be the `metadata` event that carries usage.
    pub fn push_partial(&mut self, chunk: &[u8]) -> (Vec<Frame>, Option<ProviderError>) {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            let decoded = match self.inner.decode_frame(&mut self.buffer) {
                Ok(decoded) => decoded,
                Err(e) => {
                    let error = decode_error(format!("invalid event-stream frame: {e}"));
                    return (frames, Some(error));
                }
            };
            match decoded {
                DecodedFrame::Incomplete => break,
                DecodedFrame::Complete(message) => match lift(&message) {
                    Ok(frame) => frames.push(frame),
                    Err(error) => return (frames, Some(error)),
                },
            }
        }
        if self.buffer.len() > MAX_BUFFERED_BYTES {
            let error = decode_error(format!(
                "an event-stream frame exceeded {MAX_BUFFERED_BYTES} bytes without completing"
            ));
            return (frames, Some(error));
        }
        (frames, None)
    }
}

fn decode_error(reason: String) -> ProviderError {
    ProviderError::Decode {
        adapter: ADAPTER,
        reason,
    }
}

fn header_text(message: &Message, name: &str) -> Option<String> {
    message
        .headers()
        .iter()
        .find(|h| h.name().as_str() == name)
        .and_then(|h| match h.value() {
            HeaderValue::String(s) => Some(s.as_str().to_owned()),
            _ => None,
        })
}

fn lift(message: &Message) -> Result<Frame, ProviderError> {
    let payload = message.payload().to_vec();
    match header_text(message, ":message-type").as_deref() {
        Some("event") => {
            let name = header_text(message, ":event-type")
                .ok_or_else(|| decode_error("an event frame has no `:event-type`".to_owned()))?;
            Ok(Frame::Event { name, payload })
        }
        Some("exception") => Ok(Frame::Exception {
            kind: header_text(message, ":exception-type").unwrap_or_else(|| "unknown".to_owned()),
            payload,
        }),
        Some("error") => Ok(Frame::Error {
            code: header_text(message, ":error-code").unwrap_or_else(|| "unknown".to_owned()),
            message: header_text(message, ":error-message").unwrap_or_default(),
        }),
        other => Err(decode_error(format!(
            "an event-stream frame has message type {other:?}"
        ))),
    }
}

/// Test support: frame events the way the service does, with the crate's own
/// marshaller so the bytes do not depend on this decoder.
#[cfg(test)]
pub mod testing {
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};

    fn string(name: &'static str, value: &str) -> Header {
        Header::new(name, HeaderValue::String(value.to_owned().into()))
    }

    pub fn event(name: &str, json: &str) -> Vec<u8> {
        frame(
            vec![
                string(":event-type", name),
                string(":content-type", "application/json"),
                string(":message-type", "event"),
            ],
            json,
        )
    }

    pub fn exception(kind: &str, json: &str) -> Vec<u8> {
        frame(
            vec![
                string(":exception-type", kind),
                string(":content-type", "application/json"),
                string(":message-type", "exception"),
            ],
            json,
        )
    }

    fn frame(headers: Vec<Header>, json: &str) -> Vec<u8> {
        let message = Message::new_from_parts(headers, json.as_bytes().to_vec());
        let mut out = Vec::new();
        write_message_to(&message, &mut out).expect("marshal");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{event, exception};
    use super::*;

    #[test]
    fn a_frame_split_at_every_byte_still_decodes() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        wire.extend(event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hi"}}"#,
        ));
        let mut decoder = EventStreamDecoder::new();
        let mut frames = Vec::new();
        for byte in &wire {
            frames.extend(decoder.push(std::slice::from_ref(byte)).expect("push"));
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(decoder.pending_bytes(), 0);
        match &frames[0] {
            Frame::Event { name, payload } => {
                assert_eq!(name, "messageStart");
                assert_eq!(payload, br#"{"role":"assistant"}"#);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn several_frames_in_one_chunk_all_decode() {
        let mut wire = event("messageStart", "{}");
        wire.extend(event("messageStop", r#"{"stopReason":"end_turn"}"#));
        wire.extend(event("metadata", "{}"));
        let frames = EventStreamDecoder::new().push(&wire).expect("push");
        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn a_corrupted_byte_fails_the_checksum_loudly() {
        let mut wire = event("messageStart", r#"{"role":"assistant"}"#);
        let last = wire.len() - 6;
        wire[last] ^= 0xff;
        let err = EventStreamDecoder::new()
            .push(&wire)
            .expect_err("bad crc must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[test]
    fn an_exception_frame_is_lifted_with_its_type() {
        let wire = exception("throttlingException", r#"{"message":"Too many requests"}"#);
        let frames = EventStreamDecoder::new().push(&wire).expect("push");
        match &frames[0] {
            Frame::Exception { kind, payload } => {
                assert_eq!(kind, "throttlingException");
                assert!(String::from_utf8_lossy(payload).contains("Too many"));
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn a_partial_frame_stays_pending() {
        let wire = event("messageStart", "{}");
        let mut decoder = EventStreamDecoder::new();
        let frames = decoder.push(&wire[..wire.len() - 3]).expect("push");
        assert!(frames.is_empty());
        assert!(decoder.pending_bytes() > 0);
    }
}
