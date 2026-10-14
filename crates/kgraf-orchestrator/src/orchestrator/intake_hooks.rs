//! The loop's side of the intake table and the snapshot it publishes
//! (decision 0100, section 4; the mechanism is described in `run_intake.rs`).
//!
//! **One appender per port.** [`serve_port`] runs in the port's reader task
//! (the box's channel port and the CLI's stdin port both come through
//! `Orchestrator::attach_port`). It appends a note on the intake's own
//! connection, acknowledges once the row is committed, and wakes the loop with
//! an in-process message. It never waits for the loop: a full channel only
//! leaves the wake to the loop's next step, which folds every row past the last
//! one anyway.
//!
//! **One record writer.** Only the loop folds rows into the record, and it
//! saves the last folded row id in the same record write as what the fold
//! changed.

use super::{LoopMsg, OrchestratorError, RunLoop, blocking};
use crate::port::{InboundKind, InboundMessage, MessagePort, OutboundReply};
use crate::run_intake::{AckTiming, Intake, IntakeKind, RUN_TARGET, Row, ack_text, status_text};
use crate::run_state::RecordedNote;
use crate::trace::TraceEvent;
use kgraf_tools::UntrustedText;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Serve `port` until it closes: see the module docs.
pub(super) async fn serve_port(
    intake: Arc<Intake>,
    port: Arc<dyn MessagePort>,
    tx: mpsc::Sender<LoopMsg>,
) {
    loop {
        match port.receive().await {
            Ok(Some(m)) => take_one(&intake, &port, &tx, m).await,
            Ok(None) => break,
            Err(e) => {
                let _ = tx.try_send(LoopMsg::PortError(format!("message port failed: {e}")));
                break;
            }
        }
    }
}

/// Wait until the run this reader serves is named (the loop names it as it
/// opens; a reader attached before `start` waits for that).
async fn run_of(intake: &Intake) -> kgraf_core::RunId {
    let mut rx = intake.subscribe();
    loop {
        if let Some(run) = rx.borrow_and_update().run.clone() {
            return run;
        }
        if rx.changed().await.is_err() {
            // The sender lives in `intake`, which this reader holds: never
            // closed while it waits.
            std::future::pending::<()>().await;
        }
    }
}

async fn take_one(
    intake: &Arc<Intake>,
    port: &Arc<dyn MessagePort>,
    tx: &mpsc::Sender<LoopMsg>,
    m: InboundMessage,
) {
    let received = std::time::Instant::now();
    let (text, acked) = match m.kind {
        InboundKind::StatusQuestion => (status_text(&intake.snapshot()), None),
        InboundKind::Note => {
            let run = run_of(intake).await;
            let row = Row {
                run: run.to_string(),
                kind: IntakeKind::Note,
                target: RUN_TARGET.to_owned(),
                channel: m.channel.clone(),
                message_id: m.id.clone(),
                received_ms: m.received_ms,
                // Scrubbed at intake (decision 0071).
                body: m.text.for_storage().into_stored_string(),
            };
            match intake.append(row).await {
                Ok(id) => {
                    let committed = std::time::Instant::now();
                    let text = ack_text(&intake.snapshot(), tx.is_closed());
                    (text.to_owned(), Some((id, committed)))
                }
                Err(e) => {
                    let _ = tx.try_send(LoopMsg::PortError(format!(
                        "the note {} was not taken in: {e}",
                        m.id
                    )));
                    (format!("not noted: {e}"), None)
                }
            }
        }
    };
    let sent = port
        .reply(OutboundReply {
            in_reply_to: m.id.clone(),
            text,
        })
        .await;
    if let Some((row, committed)) = acked {
        let ack_us = i64::try_from(received.elapsed().as_micros()).unwrap_or(i64::MAX);
        intake.note_ack(AckTiming {
            row,
            ack_us,
            committed,
        });
        // The wake: a full channel or an ended loop loses nothing, since the
        // loop folds every row past the last one at its next step or resume.
        let _ = tx.try_send(LoopMsg::Intake);
    }
    if let Err(e) = sent {
        // A reply that did not go out is recorded, not assumed.
        let _ = tx.try_send(LoopMsg::PortError(format!(
            "reply to {} not delivered: {e}",
            m.id
        )));
    }
}

impl RunLoop {
    /// Publish the run's state for readers that must not wait for the loop:
    /// a status question, the box's question cards (decision 0100).
    pub(super) fn publish_snapshot(&self) {
        let status = self.status();
        let questions = self.question_desk.pending();
        let at_ms = self.now();
        self.intake.publish(|s| {
            s.status = Some(status);
            s.questions = questions;
            s.at_ms = at_ms;
        });
    }

    /// The loop is running this run (`true`) or no longer is.
    pub(super) fn publish_live(&self, live: bool) {
        self.intake.publish(|s| s.live = live);
    }

    /// The planner call the loop now waits on, for a status answer given
    /// during it.
    pub(super) fn publish_planning(&self, what: Option<&str>) {
        let what = what.map(str::to_owned);
        self.intake.publish(|s| s.planning = what);
    }

    /// Fold every intake row of this run past the last one folded into the
    /// record, and save the record with the new last row id: one record
    /// write, so a kill folds each row once or not at all. A table that
    /// cannot be read is said and retried at the next step.
    pub(super) async fn fold_intake(&mut self) -> Result<(), OrchestratorError> {
        if self.run_lease_lost {
            return Ok(());
        }
        let run = self.record.run.to_string();
        let after = self.record.intake_folded.unwrap_or(0);
        let intake = self.intake.clone();
        let rows = match blocking(|| intake.rows_after(&run, after)) {
            Ok(rows) => rows,
            Err(e) => {
                let detail = format!("the intake table could not be read; retried: {e}");
                if self.notices.insert(detail.clone()) {
                    let at_ms = self.now();
                    self.trace(TraceEvent::Notice {
                        at_ms,
                        contract: String::new(),
                        worker: String::new(),
                        detail,
                    })
                    .await?;
                }
                return Ok(());
            }
        };
        let last = rows.last().map_or(after, |r| r.id);
        // Rows past the watermark already taken in (notes, used or dropped
        // planner answers): kept in the record until the watermark passes
        // them, so none is folded twice.
        let mut above: std::collections::BTreeSet<i64> =
            self.record.intake_above.iter().copied().collect();
        let mut unresolved: Option<i64> = None;
        let mut folded = Vec::with_capacity(rows.len());
        for row in rows {
            if above.contains(&row.id) {
                continue;
            }
            let kind = IntakeKind::parse(&row.kind);
            match kind {
                Some(IntakeKind::Note) => {
                    let n = self.record.notes.len();
                    self.record.notes.push(RecordedNote {
                        id: if row.message_id.is_empty() {
                            format!("note-{n}")
                        } else {
                            row.message_id.clone()
                        },
                        channel: row.channel.clone(),
                        received_ms: row.received_ms,
                        kind: InboundKind::Note,
                        // Already scrubbed by the reader that appended it.
                        text: UntrustedText::new(row.body.clone()),
                        delivered: false,
                    });
                }
                // An answer not yet used holds the watermark below it, so a
                // kill before its use leaves it to the next resume.
                Some(IntakeKind::PlannerAnswer) => {
                    if !self.fold_planner_answer(&row)? {
                        unresolved.get_or_insert(row.id);
                        continue;
                    }
                }
                // Written by no part of this build: kept in the table, said.
                Some(IntakeKind::Change | IntakeKind::NodeMessage | IntakeKind::Answer) | None => {
                    self.notices.insert(format!(
                        "intake row {} of kind `{}` is not taken in by this build; it stays in \
                         the table",
                        row.id, row.kind
                    ));
                }
            }
            above.insert(row.id);
            folded.push((row.id, row.kind));
        }
        let watermark = unresolved.map_or(last, |u| (u - 1).max(after));
        let kept: Vec<i64> = above.into_iter().filter(|r| *r > watermark).collect();
        if !folded.is_empty() || watermark != after || kept != self.record.intake_above {
            self.record.intake_folded = Some(watermark);
            self.record.intake_above = kept;
            self.save()?;
        }
        let saved = std::time::Instant::now();
        // The reader notes its measure after the reply has gone out, so a
        // measure can come after its row's fold: it is emitted by the next
        // fold. Measures of rows past this fold wait for theirs.
        let (ready, later): (Vec<AckTiming>, Vec<AckTiming>) = self
            .intake
            .take_acks()
            .into_iter()
            .partition(|a| a.row <= last);
        for a in later {
            self.intake.note_ack(a);
        }
        if !self.latency.enabled {
            return Ok(());
        }
        for a in &ready {
            self.emit_latency(
                crate::latency::LatencyKind::NoteAck,
                crate::latency::attribute(a.ack_us, &[], &[]),
                None,
            )
            .await?;
        }
        for (row, kind) in folded {
            let ack = ready.iter().find(|a| a.row == row);
            let at_ms = self.now();
            let fold_us = ack.map(|a| {
                i64::try_from(saved.duration_since(a.committed).as_micros()).unwrap_or(i64::MAX)
            });
            self.write_latency_event(TraceEvent::IntakeFolded {
                at_ms,
                row,
                kind,
                ack_us: ack.map(|a| a.ack_us),
                fold_us,
            })
            .await;
        }
        Ok(())
    }

    /// Whether a reader acknowledged a row the loop has not folded yet: the
    /// loop folds at its next step even if the wake was lost.
    pub(super) fn intake_waiting(&self) -> bool {
        self.intake.has_acks()
    }
}
