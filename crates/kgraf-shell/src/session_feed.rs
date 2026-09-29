//! The session graph, off the UI thread (decision 0067).
//!
//! `kgraf_graph::sessions::read_session_graph` is a SQLite read; the galaxy needs it
//! applied every frame the stage is live, so this tails the graph's change-log
//! watermark on a background thread and reads the session graph again only when the
//! watermark moved, the same shape [`crate::activity::ActivityWatch`] already uses for
//! the live activity card (0063). An idle project costs one indexed `MAX(seq)` read per
//! poll, never a session read, and never a read on the UI thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use kgraf_graph::sessions::{SessionGraph, read_session_graph};
use kgraf_graph::snapshot::{ProjectIndex, open_existing};

/// How often the watch reads the log's watermark; the same cadence as the activity
/// card, since both are change-log tails of the same store.
pub const POLL: Duration = crate::activity::POLL;

/// What the watch reports. `SessionGraph` carries no `Debug` of its own (it is a read,
/// not a value meant for comparison or display), so this implements it by hand rather
/// than adding a derive to a struct another unit owns.
#[derive(Clone)]
pub enum SessionUpdate {
    /// The session graph, freshly read.
    Graph(Arc<SessionGraph>),
    /// The log cannot be read (no index yet, another schema, a read error), said until
    /// it can.
    Unavailable(String),
}

impl std::fmt::Debug for SessionUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graph(g) => write!(
                f,
                "Graph({} sessions, {} spawned, {} connections)",
                g.nodes.len(),
                g.spawned.len(),
                g.connections.len()
            ),
            Self::Unavailable(e) => write!(f, "Unavailable({e:?})"),
        }
    }
}

/// A thread tailing one project's change log for its session graph. Dropping it stops
/// and joins the thread.
pub struct SessionFeed {
    rx: Receiver<SessionUpdate>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SessionFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionFeed").finish_non_exhaustive()
    }
}

impl Drop for SessionFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

impl SessionFeed {
    /// Tails `root`'s change log every `poll`; `wake` runs after each update.
    pub fn spawn(
        root: PathBuf,
        poll: Duration,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("session-feed".into())
            .spawn(move || {
                let mut last_err: Option<String> = None;
                let mut watermark: Option<i64> = None;
                let mut store = None;
                while !flag.load(Ordering::SeqCst) {
                    match read(&root, &mut store, &mut watermark) {
                        Ok(Some(graph)) => {
                            last_err = None;
                            if tx.send(SessionUpdate::Graph(Arc::new(graph))).is_err() {
                                return;
                            }
                            wake();
                        }
                        Ok(None) => {}
                        Err(e) if last_err.as_deref() != Some(e.as_str()) => {
                            last_err = Some(e.clone());
                            if tx.send(SessionUpdate::Unavailable(e)).is_err() {
                                return;
                            }
                            wake();
                        }
                        Err(_) => {}
                    }
                    std::thread::park_timeout(poll);
                }
            })?;
        Ok(Self {
            rx,
            stop,
            thread: Some(thread),
        })
    }

    /// The newest update, if one arrived; older ones are superseded.
    pub fn latest(&self) -> Option<SessionUpdate> {
        self.rx.try_iter().last()
    }
}

/// The log's watermark and, when it moved, every run's session graph. The store is
/// kept open between polls and reopened after an error, as `activity.rs`'s does.
fn read(
    root: &Path,
    store: &mut Option<kgraf_graph::GraphStore>,
    watermark: &mut Option<i64>,
) -> Result<Option<SessionGraph>, String> {
    if store.is_none() {
        *watermark = None;
        *store = match open_existing(root).map_err(|e| e.to_string())? {
            ProjectIndex::Ready(s) => Some(s),
            ProjectIndex::Missing { db_path } => {
                return Err(format!("no index at {}", db_path.display()));
            }
            ProjectIndex::OtherSchema {
                found, expected, ..
            } => {
                return Err(format!(
                    "the index has schema {found:?}; this build reads {expected}"
                ));
            }
        };
    }
    let Some(s) = store.as_ref() else {
        return Err("the index is not open".to_owned());
    };
    let result = s
        .change_watermark()
        .and_then(|w| {
            if *watermark == Some(w) {
                return Ok(None);
            }
            *watermark = Some(w);
            read_session_graph(s, None).map(Some)
        })
        .map_err(|e| e.to_string());
    if result.is_err() {
        *store = None;
    }
    result
}
