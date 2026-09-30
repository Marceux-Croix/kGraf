//! The event tail.
//!
//! Decision 0007: database triggers append to a change log, one poller tails it
//! with a watermark, and **that change log is the only event source**.
//!
//! The property that matters is not efficiency. It is that no code path can
//! mutate state without the interface hearing about it. A live graph driven by
//! hand-placed event emissions will eventually miss one, and the missed emission
//! is invisible until a user notices the graph has gone stale. Because the
//! triggers live on the tables, code written later, by a person or by one of
//! kGraf's own workers, inherits the guarantee without knowing it exists.

use crate::error::Result;
use crate::store::GraphStore;
use rusqlite::params;

/// What happened to a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
}

impl ChangeOp {
    fn parse(name: &str) -> Option<ChangeOp> {
        Some(match name {
            "insert" => ChangeOp::Insert,
            "update" => ChangeOp::Update,
            "delete" => ChangeOp::Delete,
            _ => return None,
        })
    }

    /// The op's stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeOp::Insert => "insert",
            ChangeOp::Update => "update",
            ChangeOp::Delete => "delete",
        }
    }
}

/// One change-log entry, which is one event.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Change {
    /// Monotonic sequence number. The watermark is a value of this.
    pub seq: i64,
    /// Milliseconds since the epoch.
    pub ts: i64,
    pub table: String,
    pub op: ChangeOp,
    pub row_id: String,
    /// Enough of the row to render the event without a second query.
    pub payload: Option<serde_json::Value>,
}

/// A change-log row that could not be read whole. An unknown op (a newer build
/// wrote an op this one does not know) drops the row from the batch; a payload
/// that is not JSON keeps the row with no payload. Either way the reader goes on
/// and the fault is reported, so one row never stops the tail for the life of the
/// database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeFault {
    pub seq: i64,
    /// Milliseconds since the epoch, as the row recorded it.
    pub ts: i64,
    pub table: String,
    pub reason: String,
    /// Whether the row is still in the batch (payload fault) or was dropped.
    pub kept: bool,
}

/// Changes read together with the rows that could not be read whole.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChangePage {
    pub changes: Vec<Change>,
    pub faults: Vec<ChangeFault>,
    /// The highest sequence number read, damaged rows included.
    last_seq: Option<i64>,
}

/// A cursor over the change log.
///
/// Holds a watermark rather than a subscription, so a UI that was closed and
/// reopened, or a process that crashed, resumes exactly where it stopped instead
/// of missing the window it was away for.
pub struct EventTail {
    watermark: i64,
    faults: Vec<ChangeFault>,
}

impl EventTail {
    /// Start from the beginning of the log.
    pub fn from_start() -> Self {
        Self {
            watermark: 0,
            faults: Vec::new(),
        }
    }

    /// Start from a recorded watermark, which is what resuming looks like.
    pub fn resume_from(watermark: i64) -> Self {
        Self {
            watermark,
            faults: Vec::new(),
        }
    }

    /// Start from the current end of the log, ignoring history.
    pub fn from_now(store: &GraphStore) -> Result<Self> {
        Ok(Self {
            watermark: store.change_watermark()?,
            faults: Vec::new(),
        })
    }

    /// The last sequence number this tail has delivered.
    pub fn watermark(&self) -> i64 {
        self.watermark
    }

    /// Rows `next_batch` could not read whole since the last call to this
    /// method, drained. The caller reports them.
    pub fn take_faults(&mut self) -> Vec<ChangeFault> {
        std::mem::take(&mut self.faults)
    }

    /// The newest `limit` changes, newest first, without moving any watermark.
    ///
    /// The desktop shell's live activity card (decision 0063) reads the change
    /// log's tail with this. It is a reader of the one event source, not a second
    /// source: the rows are the log's own. The read walks the primary key backward,
    /// so its cost follows `limit`, not the log's length.
    pub fn newest(store: &GraphStore, limit: usize) -> Result<Vec<Change>> {
        Ok(Self::newest_page(store, limit)?.changes)
    }

    /// [`Self::newest`] with the rows that could not be read whole.
    pub fn newest_page(store: &GraphStore, limit: usize) -> Result<ChangePage> {
        let conn = store.conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, table_name, op, row_id, payload
             FROM change_log ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], read_row)?;
        collect_page(rows)
    }

    /// Rows after `after_seq` whose op is not one the log's triggers write,
    /// oldest first, as faults. The filtered reads ([`Self::newest_of_page`])
    /// select by op and so never return such a row; this walks the primary key
    /// range after `after_seq`, so its cost follows the rows written since.
    pub fn unknown_ops_since(store: &GraphStore, after_seq: i64) -> Result<Vec<ChangeFault>> {
        let conn = store.conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, table_name, op, row_id, payload
             FROM change_log
             WHERE seq > ?1 AND op NOT IN ('insert', 'update', 'delete')
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![after_seq], read_row)?;
        Ok(collect_page(rows)?.faults)
    }

    /// The newest `limit` changes of the given tables and operations, newest
    /// first. The filter is applied in SQL before the limit, so rows of other
    /// tables (a reindex writes tens of thousands of layout moves) never fill
    /// the tail. Each entry is a table and the operations to keep from it; an
    /// empty operation list keeps every operation.
    ///
    /// One `ORDER BY seq DESC LIMIT n` per (table, operation), each a backward
    /// range of the `change_log_table` index, then the newest `n` of their union.
    /// A single OR filter scanned and sorted every kept row before
    /// its limit: 56 ms at 300,000 lease events, and the log is never pruned.
    /// Now the cost follows `n` times the number of pairs.
    pub fn newest_of(
        store: &GraphStore,
        limit: usize,
        keep: &[(&str, &[ChangeOp])],
    ) -> Result<Vec<Change>> {
        Ok(Self::newest_of_page(store, limit, keep)?.changes)
    }

    /// [`Self::newest_of`] with the rows that could not be read whole.
    pub fn newest_of_page(
        store: &GraphStore,
        limit: usize,
        keep: &[(&str, &[ChangeOp])],
    ) -> Result<ChangePage> {
        let Some((sql, args)) = newest_sql(limit, keep) else {
            return Ok(ChangePage::default());
        };
        let conn = store.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), read_row)?;
        collect_page(rows)
    }

    /// Deliver up to `limit` changes and advance the watermark past them.
    ///
    /// The watermark advances only over rows actually returned, so a caller that
    /// crashes between the read and its own handling redelivers rather than
    /// skipping. At-least-once is the right choice here: a duplicated graph
    /// update is idempotent, a dropped one leaves the picture wrong.
    pub fn next_batch(&mut self, store: &GraphStore, limit: usize) -> Result<Vec<Change>> {
        let conn = store.conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, table_name, op, row_id, payload
             FROM change_log WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![self.watermark, limit as i64], read_row)?;
        let page = collect_page(rows)?;
        // The watermark passes a damaged row too: stopping at it would stall the
        // tail on that row for good.
        if let Some(last) = page.last_seq {
            self.watermark = last;
        }
        self.faults.extend(page.faults);
        Ok(page.changes)
    }
}

/// The SQL and its arguments for [`EventTail::newest_of`]; `None` for nothing
/// kept. `?1` is the limit.
fn newest_sql(limit: usize, keep: &[(&str, &[ChangeOp])]) -> Option<(String, Vec<String>)> {
    const ALL: [ChangeOp; 3] = [ChangeOp::Insert, ChangeOp::Update, ChangeOp::Delete];
    let mut parts = Vec::new();
    let mut args: Vec<String> = vec![limit.to_string()];
    for (table, ops) in keep {
        let ops: &[ChangeOp] = if ops.is_empty() { &ALL } else { ops };
        for op in ops {
            args.push((*table).to_owned());
            args.push(op.as_str().to_owned());
            parts.push(format!(
                "SELECT * FROM (SELECT seq, ts, table_name, op, row_id, payload
                 FROM change_log WHERE table_name = ?{} AND op = ?{}
                 ORDER BY seq DESC LIMIT CAST(?1 AS INTEGER))",
                args.len() - 1,
                args.len()
            ));
        }
    }
    (!parts.is_empty()).then(|| {
        (
            format!(
                "{} ORDER BY seq DESC LIMIT CAST(?1 AS INTEGER)",
                parts.join(" UNION ALL ")
            ),
            args,
        )
    })
}

/// One row as read: the change if it is usable, and the fault if it is not whole.
struct ReadRow {
    seq: i64,
    change: Option<Change>,
    fault: Option<ChangeFault>,
}

fn collect_page(rows: impl Iterator<Item = rusqlite::Result<ReadRow>>) -> Result<ChangePage> {
    let mut page = ChangePage::default();
    for row in rows {
        let row = row?;
        page.last_seq = Some(page.last_seq.map_or(row.seq, |s| s.max(row.seq)));
        page.changes.extend(row.change);
        page.faults.extend(row.fault);
    }
    Ok(page)
}

/// One `change_log` row (columns seq, ts, table_name, op, row_id, payload). The
/// log's triggers write only insert, update and delete. Any other op text is a
/// damaged row (or a newer build's): dropped and reported, not read as an update
/// and not fatal to the rows around it.
fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReadRow> {
    let seq: i64 = row.get(0)?;
    let table: String = row.get(2)?;
    let op: String = row.get(3)?;
    let payload: Option<String> = row.get(5)?;
    let Some(op) = ChangeOp::parse(&op) else {
        return Ok(ReadRow {
            seq,
            change: None,
            fault: Some(ChangeFault {
                seq,
                ts: row.get(1)?,
                table,
                reason: format!("change_log holds the unknown op `{op}`"),
                kept: false,
            }),
        });
    };
    let mut fault = None;
    let payload = match payload {
        None => None,
        Some(text) => match serde_json::from_str(&text) {
            Ok(value) => Some(value),
            Err(error) => {
                fault = Some(ChangeFault {
                    seq,
                    ts: row.get(1)?,
                    table: table.clone(),
                    reason: format!("change_log payload is not JSON: {error}"),
                    kept: true,
                });
                None
            }
        },
    };
    Ok(ReadRow {
        seq,
        change: Some(Change {
            seq,
            ts: row.get(1)?,
            table,
            op,
            row_id: row.get(4)?,
            payload,
        }),
        fault,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Indexer;
    use std::fs;

    /// The query plan of `EventTail::newest_of`, one line per step: each part
    /// must walk the index, never scan the log.
    fn newest_of_plan(store: &GraphStore, keep: &[(&str, &[ChangeOp])]) -> Result<String> {
        let Some((sql, args)) = newest_sql(100, keep) else {
            return Ok(String::new());
        };
        let conn = store.conn();
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            r.get::<_, String>(3)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.join("\n"))
    }

    fn tree() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        (dir, store)
    }

    fn write(dir: &tempfile::TempDir, rel: &str, body: &str) {
        let full = dir.path().join(rel);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, body).expect("write");
    }

    #[test]
    fn indexing_emits_events_without_any_hand_placed_emission() {
        // The indexer never calls an event API. The triggers do it, which is why
        // this holds for any future writer too.
        let (dir, store) = tree();
        write(&dir, "src/lib.rs", "fn a() {}\n");
        let mut tail = EventTail::from_start();
        Indexer::new(&store).index().expect("index");
        let batch = tail.next_batch(&store, 1000).expect("batch");
        assert!(
            batch
                .iter()
                .any(|c| c.table == "files" && c.op == ChangeOp::Insert)
        );
        assert!(
            batch
                .iter()
                .any(|c| c.table == "nodes" && c.op == ChangeOp::Insert)
        );
        assert!(batch.iter().any(|c| c.table == "edges"));
    }

    #[test]
    fn a_raw_sql_write_is_still_heard() {
        // The point of trigger-based capture: a code path that knows nothing
        // about events cannot escape them. This mutation bypasses every method
        // on `GraphStore` and is still logged.
        let (dir, store) = tree();
        write(&dir, "src/lib.rs", "fn a() {}\n");
        Indexer::new(&store).index().expect("index");
        let mut tail = EventTail::from_now(&store).expect("tail");

        store
            .conn()
            .execute(
                "UPDATE nodes SET signature = 'edited behind the API' WHERE kind = 'function'",
                [],
            )
            .expect("raw update");

        let batch = tail.next_batch(&store, 100).expect("batch");
        assert!(
            batch
                .iter()
                .any(|c| c.table == "nodes" && c.op == ChangeOp::Update),
            "a write that bypassed the API was not logged: {batch:#?}"
        );
    }

    #[test]
    fn a_watermark_resumes_exactly_where_it_stopped() {
        let (dir, store) = tree();
        write(&dir, "src/a.rs", "fn a() {}\n");
        let indexer = Indexer::new(&store);
        indexer.index().expect("first");

        let mut tail = EventTail::from_start();
        let first = tail.next_batch(&store, 2).expect("batch");
        assert_eq!(first.len(), 2);
        let saved = tail.watermark();

        write(&dir, "src/b.rs", "fn b() {}\n");
        indexer.index().expect("second");

        // A fresh tail resuming from the saved watermark sees everything after
        // it and nothing before, including the events from while it was away.
        let mut resumed = EventTail::resume_from(saved);
        let rest = resumed.next_batch(&store, 1000).expect("batch");
        assert!(rest.iter().all(|c| c.seq > saved));
        assert!(
            rest.iter().any(|c| c
                .payload
                .as_ref()
                .and_then(|p| p.get("path"))
                .and_then(|p| p.as_str())
                == Some("src/b.rs")),
            "the window the tail was away for was lost"
        );
    }

    #[test]
    fn an_idle_tail_yields_nothing_and_does_not_advance() {
        let (dir, store) = tree();
        write(&dir, "src/a.rs", "fn a() {}\n");
        Indexer::new(&store).index().expect("index");
        let mut tail = EventTail::from_now(&store).expect("tail");
        let before = tail.watermark();
        assert!(tail.next_batch(&store, 100).expect("batch").is_empty());
        assert_eq!(tail.watermark(), before);
    }

    #[test]
    fn deletions_are_events_too() {
        let (dir, store) = tree();
        write(&dir, "src/a.rs", "fn a() {}\n");
        write(&dir, "src/b.rs", "fn b() {}\n");
        let indexer = Indexer::new(&store);
        indexer.index().expect("index");
        let mut tail = EventTail::from_now(&store).expect("tail");

        fs::remove_file(dir.path().join("src/b.rs")).expect("remove");
        indexer.index().expect("reindex");

        let batch = tail.next_batch(&store, 1000).expect("batch");
        assert!(
            batch
                .iter()
                .any(|c| c.table == "files" && c.op == ChangeOp::Delete),
            "a removed file must be announced: {batch:#?}"
        );
        assert!(
            batch
                .iter()
                .any(|c| c.table == "nodes" && c.op == ChangeOp::Delete),
            "the cascade must be announced too"
        );
    }

    #[test]
    fn newest_reads_the_tail_newest_first_and_moves_no_watermark() {
        let (dir, store) = tree();
        write(&dir, "src/a.rs", "fn a() {}\n");
        Indexer::new(&store).index().expect("index");
        let mut tail = EventTail::from_start();
        let all = tail.next_batch(&store, 100_000).expect("all");
        let newest = EventTail::newest(&store, 3).expect("newest");
        assert_eq!(newest.len(), 3);
        let expect: Vec<i64> = all.iter().rev().take(3).map(|c| c.seq).collect();
        assert_eq!(newest.iter().map(|c| c.seq).collect::<Vec<_>>(), expect);
        assert_eq!(newest[0], *all.last().expect("rows"));
        // A tail is untouched by it.
        let fresh = EventTail::from_start();
        assert_eq!(fresh.watermark(), 0);
        assert_eq!(tail.watermark(), all.last().expect("rows").seq);
    }

    /// On a large log (300,000 lease events, most of them
    /// renewals) every part of the query walks the index, none scans the log,
    /// and the newest kept rows come back in time order.
    #[test]
    fn a_change_row_with_an_unknown_op_is_skipped_and_reported_not_read_as_an_update() {
        let (_dir, store) = tree();
        let conn = store.conn();
        for (op, payload) in [
            ("insert", None),
            ("upsert", None),
            ("update", Some("{not json")),
            ("update", None),
        ] {
            conn.execute(
                "INSERT INTO change_log(table_name, op, row_id, payload)
                 VALUES ('nodes', ?1, 'r1', ?2)",
                params![op, payload],
            )
            .expect("insert");
        }
        let page = EventTail::newest_page(&store, 10).expect("page");
        assert_eq!(
            page.changes.len(),
            3,
            "the rows around the bad one are read"
        );
        assert_eq!(page.faults.len(), 2, "{:?}", page.faults);
        assert!(
            page.faults
                .iter()
                .any(|f| !f.kept && f.reason.contains("upsert"))
        );
        assert!(
            page.faults
                .iter()
                .any(|f| f.kept && f.reason.contains("JSON"))
        );

        // The tail passes the damaged rows and keeps going for later rows.
        let mut tail = EventTail::from_start();
        let batch = tail.next_batch(&store, 100).expect("batch");
        assert_eq!(batch.len(), 3);
        assert_eq!(tail.watermark(), 4, "the watermark stopped at a bad row");
        assert_eq!(tail.take_faults().len(), 2);
        assert!(tail.take_faults().is_empty(), "faults drain");
        conn.execute(
            "INSERT INTO change_log(table_name, op, row_id, payload)
             VALUES ('nodes', 'insert', 'r9', NULL)",
            [],
        )
        .expect("insert");
        assert_eq!(tail.next_batch(&store, 100).expect("batch").len(), 1);
    }

    #[test]
    fn newest_of_walks_the_index_on_a_large_log() {
        let (_dir, store) = tree();
        let conn = store.conn();
        conn.execute_batch("BEGIN").expect("begin");
        {
            let mut ins = conn
                .prepare(
                    "INSERT INTO change_log(table_name, op, row_id, payload)
                     VALUES (?1, ?2, ?3, NULL)",
                )
                .expect("prepare");
            for i in 0..300_000u32 {
                let (table, op) = match i % 100 {
                    0 => ("leases", "insert"),
                    1 => ("leases", "delete"),
                    2 => ("work_nodes", "insert"),
                    _ => ("leases", "update"),
                };
                ins.execute(params![table, op, format!("r{i}")])
                    .expect("insert");
            }
        }
        conn.execute_batch("COMMIT").expect("commit");
        let keep: [(&str, &[ChangeOp]); 2] = [
            ("leases", &[ChangeOp::Insert, ChangeOp::Delete]),
            ("work_nodes", &[]),
        ];
        let plan = newest_of_plan(&store, &keep).expect("plan");
        assert!(
            plan.contains("change_log_table_op") && !plan.contains("SCAN change_log"),
            "{plan}"
        );
        let t0 = std::time::Instant::now();
        let rows = EventTail::newest_of(&store, 100, &keep).expect("rows");
        let took = t0.elapsed();
        assert_eq!(rows.len(), 100);
        assert!(rows.windows(2).all(|w| w[0].seq > w[1].seq));
        assert!(
            rows.iter()
                .all(|c| c.op != ChangeOp::Update || c.table != "leases")
        );
        // Loose: the bound catches a scan of 300,000 rows (tens of ms), not noise.
        assert!(took < std::time::Duration::from_millis(40), "{took:?}");
    }

    /// The filter applies before the limit: the tail of kept rows is found
    /// under thousands of newer excluded rows.
    #[test]
    fn newest_of_filters_before_the_limit() {
        let (dir, store) = tree();
        write(&dir, "src/a.rs", "fn a() {}\nfn b() { a(); }\n");
        Indexer::new(&store).index().expect("index");
        let kept =
            EventTail::newest_of(&store, 5, &[("files", &[ChangeOp::Insert])]).expect("files");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].table, "files");
        // Newer excluded rows do not push it out.
        for i in 0..50 {
            store
                .conn()
                .execute(
                    "UPDATE nodes SET signature = ?1 WHERE kind = 'function'",
                    [format!("s{i}")],
                )
                .expect("update");
        }
        let kept = EventTail::newest_of(
            &store,
            5,
            &[
                ("files", &[ChangeOp::Insert, ChangeOp::Delete]),
                ("work_nodes", &[]),
            ],
        )
        .expect("files");
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(
            EventTail::newest_of(&store, 5, &[])
                .expect("none")
                .is_empty()
        );
    }

    #[test]
    fn sequence_numbers_are_monotonic() {
        let (dir, store) = tree();
        for i in 0..5 {
            write(&dir, &format!("src/f{i}.rs"), &format!("fn f{i}() {{}}\n"));
        }
        Indexer::new(&store).index().expect("index");
        let mut tail = EventTail::from_start();
        let batch = tail.next_batch(&store, 10_000).expect("batch");
        assert!(batch.len() > 5);
        assert!(batch.windows(2).all(|w| w[0].seq < w[1].seq));
    }
}
