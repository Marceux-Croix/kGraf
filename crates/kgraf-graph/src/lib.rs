//! kGraf's code graph.
//!
//! The graph is two things at once: the picture the user looks at, and the
//! source agent context is assembled from (agents read graph slices, not whole files). Both jobs
//! are served by the same rows.
//!
//! Three properties are structural rather than incidental, and each has a test
//! that would fail if it were lost.
//!
//! **Nodes index locations, not text** (decision 0007). A node carries a path
//! and a line span; source is re-read from disk on demand. The index stays
//! small and it is impossible for the graph to serve stale source.
//!
//! **Incremental reindexing compares content hashes**, not modification time
//! and size. The reference implementation compares only time and size, so a
//! change preserving both is missed, and agents writing files rapidly is
//! exactly that workload.
//!
//! **Layout positions are a pure function of node identity, own call depth and
//! cluster population bucket**, so they are stable across a reindex, an insert
//! moves only what [`layout`] lists, and a node that did not exist at index time
//! still gets a position without a global solve (decision 0015).
//!
//! The graph is explicitly *not* the collision predictor: an early experiment falsified
//! that, and decision 0020 moved the job to the co-change relation in
//! [`cochange`].

pub mod chunk;
pub mod cochange;
pub mod error;
pub mod events;
pub mod facts;
pub mod index;
pub mod lang;
pub mod layout;
pub mod leases;
pub mod presumed;
pub mod query;
pub mod rank;
pub mod sessions;
pub mod slice;
pub mod snapshot;
pub mod store;
pub mod work;
pub mod work_schedule;

pub use chunk::{Chunk, NodeKind, chunk_source};
pub use cochange::{CoChangePair, CoChangeStats};
pub use error::{GraphError, Result};
pub use events::{Change, ChangeOp, EventTail};
pub use facts::{Assertion, Fact, NewFact};
pub use index::{BUS_FILE_TITLE, BUS_OWNED_DIRS, ExtractorChange, IndexReport, Indexer};
pub use lang::{Language, Tier};
pub use layout::{Position, position_for};
pub use leases::{
    Acquisition, Conflict, LeaseRecord, LeaseRegistry, MixedAcquisition, NodeAcquisition,
    NodeConflict, NodeLease, NodeLeaseRecord, WriteLease,
};
pub use presumed::{PresumedNode, PresumedOrigin, PresumedSync, StoredPresumed};
pub use query::{EdgeDirection, NodePredicate, Pattern, QueryMatch};
pub use rank::{RankRequest, RankedDefinition, RepoRank, rank_repository};
pub use sessions::{SessionGraph, SharedSessions, read_session_graph};
pub use slice::{ContextSlice, SliceMember, SliceReason};
pub use store::{EdgeKind, FileRecord, GraphStore, NodeRecord};
pub use work::{WorkKind, WorkNode, WorkWrite};
pub use work_schedule::{
    NewScheduleEvent, RunSpend, SCHEDULE_PLAN_ID, ScheduleEvent, WorkSchedule,
};
