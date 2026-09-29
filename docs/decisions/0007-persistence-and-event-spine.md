# Decision 0007: SQLite with durable facts, derived status, and a change-log event spine

**Status:** Accepted
**Related:** decisions 0002, 0010

## Context

### Why derived status

A design that reduces to observe, update durable facts, derive display
status, with display status never stored, keeps the interface honest. Its
durable session facts are few; everything the interface shows is computed.

For kGraf this is not a style preference. The galaxy view is the product,
and a stored status field is a second copy of the truth that will drift from
the first. Deriving it makes drift impossible, because there is exactly one
source for what the graph shows.

### Why a change log as the event spine

Change data capture, where database triggers write and a poller fans out to
the interface, is source-truth for events.

The property that matters: no code path can mutate state without the
interface hearing about it. A live graph driven by hand-placed event
emissions will eventually miss one, and the missed one is invisible until a
user notices the graph is stale.

### Why the graph stores locations, not text

Keeping the index small and re-reading source from disk at query time makes
it impossible for the graph to serve stale source, and means a file edit
invalidates spans rather than requiring content rewrites. A reference
implementation of this pattern indexes at a scale of tens of thousands of
nodes with no content column at all in its node table.

### Why SQLite rather than a graph database

An embedded Cypher-subset-over-SQLite approach has been demonstrated
indexing a large open-source kernel to millions of nodes and edges in
minutes, with queries under a millisecond. An embedded file also matches the
local-only requirement (decision 0002): no server, no daemon, nothing to
install.

### Why a runs table with a parent

A run is one addressable, persisted unit of an agent's work: the thing a
delegated task, a spawn, or a scheduled job can be looked up by, resumed
from, and reported on. Delegation sets the parent, so the tree is walkable
from any node, and that tree is already the coordination edge set the
interface must draw. It does not need to be derived separately.

## Decision

One embedded SQLite database per project holds the code graph, agent state,
run history, and messages.

**Only facts are stored. Display status is always derived at read time.**

**The graph indexes locations, not text.** A node carries a file path and a
line span. Source is re-read from disk when needed.

Database triggers append to a change log; one poller tails it with a
watermark and fans out to the interface. **That change log is the only
event source.**

Run history uses a runs table with a parent run id, so delegation forms a
walkable tree.

## Consequences

**Every state the interface shows must be derivable from stored facts.**
This is a real constraint, not a free simplification: a state a board would
have shown must be derivable and drawable, or it is not shown (decision
0003).

**Layout positions are cached in the database.** Positions must be stable
across reindexes or agents will appear to teleport.

**Incremental reindexing must compare content hashes, not just modification
time and size.** Comparing only time and size, while carrying an unused hash
column, misses a content change that preserves both. Agents writing files
rapidly is exactly the workload that hits that case.

**Snapshot on failure.** A dead worker hands the orchestrator a resumable
object rather than a stack trace.

**Fence events by generation id**, so events from a replaced worker cannot
be mistaken for live ones.

**Commit only turns whose tool calls are complete.** An incremental writer
commits at each turn end only when every tool use has a matching tool
result. Half-written tool state is the most common way an agent transcript
becomes unresumable.

**Terminate conservatively.** Failed probes are not proof of death.

**Record every message, including worker-to-worker messages that never
touched the orchestrator** (decision 0004), because the interface promises
that clicking an agent shows everything it sent or received.

## Alternatives considered

**A dedicated graph database.** Rejected: a server or a heavier embedded
dependency for a local desktop application, against evidence that SQLite is
fast enough at kernel scale.

**In-memory state with optional snapshots.** Rejected: the graph reads run
state continuously, and a crash mid-task must be recoverable.

**Hand-placed event emission.** Rejected for the missed-emission failure
above.

**Storing derived status for query speed.** Rejected; the drift cost
exceeds the query cost, and derivation has been demonstrated to be cheap.
