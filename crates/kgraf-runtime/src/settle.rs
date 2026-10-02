//! What a worker's persisted transcript holds, given its live event stream
//! (decision 0071).
//!
//! The live stream carries every `TextDelta` and `ReasoningDelta` as it
//! arrives, each persist-scrubbed on its own. A secret the provider streams
//! across two deltas is whole in neither, so each half passes the scrub, and
//! a file holding both halves on adjacent lines holds the secret. Splitting is
//! the common case, not an edge: providers cut deltas at token boundaries.
//!
//! [`TranscriptSettler`] sits between the live stream and the file. It holds
//! a round's deltas in memory and writes none of them. When the round settles,
//! the worker's [`WorkerEvent::AssistantMessage`] (the round's whole text,
//! scrubbed as one string) is written instead. When a round ends any other
//! way (a cancel, an abandoned round, the stream closing), the held text is
//! joined, scrubbed again as one string, and written as one `TextDelta` and
//! one `ReasoningDelta`, followed by a [`WorkerEvent::Redacted`] for anything
//! only the joined scrub found.
//!
//! **The live stream is not changed.** A hold-back on the live stream was
//! built once and reverted: it delayed short deltas until the round ended,
//! which broke live delivery and hung
//! `a_worker_cancelled_mid_stream_resumes_from_its_durable_transcript`. The
//! settler changes only what is written.
//!
//! **Cost.** A process that dies mid-round loses that round's partial text
//! from the file. A resume already drops an unfinished round's text (see
//! `rebuild.rs`), and the ledger, not the transcript, holds its spend, so
//! nothing a resume reads is lost. The shell reads the file, so its text
//! surface shows a round's text when the round settles, not delta by delta.
//! The file is also smaller: a settled round is stored once, not twice.

use crate::event::WorkerEvent;

/// Holds one round's streamed text until the round settles; see the module
/// docs. Feed it every live event in order with [`TranscriptSettler::push`],
/// write what it returns, and write [`TranscriptSettler::finish`]'s events
/// when the stream ends.
#[derive(Debug, Default)]
pub struct TranscriptSettler {
    text: String,
    reasoning: String,
}

impl TranscriptSettler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The events to write now for `event`, in order. Empty for a delta.
    pub fn push(&mut self, event: WorkerEvent) -> Vec<WorkerEvent> {
        match event {
            WorkerEvent::TextDelta { text } => {
                self.text.push_str(&text);
                Vec::new()
            }
            WorkerEvent::ReasoningDelta { text } => {
                self.reasoning.push_str(&text);
                Vec::new()
            }
            // The settled record replaces the held deltas outright.
            WorkerEvent::AssistantMessage { .. } => {
                self.text.clear();
                self.reasoning.clear();
                vec![event]
            }
            // Usage arrives inside a round (some providers send it first):
            // written at once, the round keeps holding.
            WorkerEvent::Usage { .. } => vec![event],
            // A tool-call wait notice (decision 0067):
            // like `Usage`, written at once rather than through the
            // `other` arm below, so it never flushes or loses a round's held
            // deltas just because it happened to land while one is open.
            WorkerEvent::Waiting { .. } | WorkerEvent::Resumed { .. } => vec![event],
            other => {
                let mut out = self.flush();
                out.push(other);
                out
            }
        }
    }

    /// The events still held, when the stream has ended.
    pub fn finish(&mut self) -> Vec<WorkerEvent> {
        self.flush()
    }

    fn flush(&mut self) -> Vec<WorkerEvent> {
        let mut out = Vec::new();
        let mut counts = kgraf_secrets::Redactions::default();
        let mut whole = |held: String| {
            let (scrubbed, found) = kgraf_secrets::scrub_persist_counted(&held);
            counts.merge(&found);
            scrubbed.into_owned()
        };
        if !self.reasoning.is_empty() {
            out.push(WorkerEvent::ReasoningDelta {
                text: whole(std::mem::take(&mut self.reasoning)),
            });
        }
        if !self.text.is_empty() {
            out.push(WorkerEvent::TextDelta {
                text: whole(std::mem::take(&mut self.text)),
            });
        }
        if !counts.is_empty() {
            out.push(WorkerEvent::Redacted {
                counts: counts.into_counts(),
            });
        }
        out
    }
}

/// The events [`TranscriptSettler`] writes for a whole live stream: what a
/// persisted transcript holds for the events a caller saw.
pub fn settle_for_disk(events: &[WorkerEvent]) -> Vec<WorkerEvent> {
    let mut settler = TranscriptSettler::new();
    let mut out: Vec<WorkerEvent> = events
        .iter()
        .flat_map(|e| settler.push(e.clone()))
        .collect();
    out.extend(settler.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CancelPhase;
    use kgraf_core::{CallId, TokenUsage};
    use kgraf_provider::StopReason;

    fn token() -> String {
        format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui")
    }

    fn delta(t: &str) -> WorkerEvent {
        WorkerEvent::TextDelta { text: t.into() }
    }

    fn usage() -> WorkerEvent {
        WorkerEvent::Usage {
            call: CallId::new("c1"),
            usage: TokenUsage::default(),
        }
    }

    fn joined(events: &[WorkerEvent]) -> String {
        events
            .iter()
            .map(|e| serde_json::to_string(e).expect("json"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_provider_signature_is_written_unchanged_and_never_scrubbed() {
        // A token can look like a secret to a pattern rule, and a changed byte
        // makes the provider refuse it, so it is written exactly as received.
        let signature = kgraf_provider::Signature::new(format!("sig_{}", token()), "adapter:model");
        let live = vec![
            delta("hello"),
            WorkerEvent::AssistantMessage {
                text: "hello".into(),
                reasoning: String::new(),
                signature: Some(signature.clone()),
            },
            WorkerEvent::Stop {
                finish_reason: None,
                reason: StopReason::EndTurn,
            },
        ];
        let disk: Vec<WorkerEvent> = settle_for_disk(&live)
            .into_iter()
            .map(WorkerEvent::scrubbed_for_persist)
            .collect();
        assert!(matches!(
            &disk[0],
            WorkerEvent::AssistantMessage { signature: Some(s), .. } if *s == signature
        ));
    }

    #[test]
    fn a_settled_round_writes_only_its_assistant_message() {
        let t = token();
        let (a, b) = t.split_at(12);
        let message = WorkerEvent::AssistantMessage {
            text: kgraf_secrets::scrub_persist(&format!("key {t}")).into_owned(),
            reasoning: String::new(),
            signature: None,
        };
        let live = vec![
            delta("key "),
            delta(a),
            delta(b),
            usage(),
            message.clone(),
            WorkerEvent::Stop {
                finish_reason: None,
                reason: StopReason::EndTurn,
            },
        ];
        let disk = settle_for_disk(&live);
        assert_eq!(
            disk,
            vec![
                usage(),
                message,
                WorkerEvent::Stop {
                    finish_reason: None,
                    reason: StopReason::EndTurn
                }
            ]
        );
        assert!(
            !joined(&disk).contains(a),
            "no half of the token is written"
        );
    }

    #[test]
    fn a_cancelled_round_writes_its_partial_text_whole_scrubbed() {
        let t = token();
        let (a, b) = t.split_at(12);
        let live = vec![
            delta("the key is "),
            delta(a),
            usage(),
            delta(b),
            WorkerEvent::Cancelled {
                phase: CancelPhase::ModelStream,
            },
        ];
        let disk = settle_for_disk(&live);
        let text = joined(&disk);
        assert!(!text.contains(&t) && !text.contains(a), "{text}");
        assert!(text.contains("[redacted:github-pat;"), "{text}");
        assert!(matches!(disk[0], WorkerEvent::Usage { .. }));
        assert!(
            matches!(&disk[1], WorkerEvent::TextDelta { text } if text.starts_with("the key is [redacted:github-pat;"))
        );
        assert!(
            matches!(&disk[2], WorkerEvent::Redacted { counts } if counts.get("github-pat") == Some(&1))
        );
        assert!(matches!(disk[3], WorkerEvent::Cancelled { .. }));
    }

    #[test]
    fn a_stream_that_closes_mid_round_flushes_on_finish() {
        let mut s = TranscriptSettler::new();
        assert!(s.push(delta("partial")).is_empty());
        assert_eq!(s.finish(), vec![delta("partial")]);
    }
}
