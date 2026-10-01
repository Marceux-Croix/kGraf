//! Permission events (decision 0094): each permission-mode change and each
//! grant, as an append-only row whose insert the change log records, so the
//! activity card shows it with no second event source (0063).

use crate::error::Result;
use crate::store::GraphStore;
use rusqlite::params;

/// One permission event to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionEventRow {
    pub at_ms: i64,
    /// The run it happened in, if any (a mode change with no run has none).
    pub run_id: Option<String>,
    /// The worker a grant went to, if any.
    pub agent: Option<String>,
    /// `mode_changed`, `plan_review_changed`, `granted`, `denied`,
    /// `plan_approved`, `plan_rejected`, `plan_changed`.
    pub event: String,
    /// One line for the card, scrubbed by the caller.
    pub line: String,
}

impl GraphStore {
    /// Append one permission event. Returns its sequence number.
    pub fn record_permission_event(&self, row: &PermissionEventRow) -> Result<i64> {
        self.conn().execute(
            "INSERT INTO permission_events (at_ms, run_id, agent, event, line)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.at_ms, row.run_id, row.agent, row.event, row.line],
        )?;
        Ok(self.conn().last_insert_rowid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventTail;

    #[test]
    fn a_permission_event_reaches_the_change_log() {
        let dir = tempfile::tempdir().expect("dir");
        let g = GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("open");
        g.record_permission_event(&PermissionEventRow {
            at_ms: 5,
            run_id: Some("r1".into()),
            agent: Some("r1/c1/w1".into()),
            event: "granted".into(),
            line: "granted `python3` to r1/c1/w1 (Auto, by mode)".into(),
        })
        .expect("record");
        let tail = EventTail::newest_of(&g, 10, &[("permission_events", &[])]).expect("tail");
        let row = tail.first().expect("one change");
        assert_eq!(row.table, "permission_events");
        let payload = row.payload.as_ref().expect("payload");
        assert_eq!(payload["event"], "granted");
        assert_eq!(payload["agent"], "r1/c1/w1");
    }
}
