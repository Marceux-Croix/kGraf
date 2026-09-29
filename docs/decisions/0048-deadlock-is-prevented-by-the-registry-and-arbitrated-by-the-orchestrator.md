# Decision 0048: The registry makes deadlock unconstructible; the orchestrator arbitrates blocks

**Status:** Accepted
**Related:** implements decision 0022's lease requirement; decisions 0004, 0007, 0014, 0015,
0016, 0017, 0024, 0039

## Decision

Two different jobs, in two different layers. Conflating them leads to the wrong thing being
built in each.

**The write-lease registry makes deadlock unconstructible.** One global table, keyed by file
path, acquires a worker's whole write set sorted and taken in a single transaction. A worker
never holds part of a set while waiting on the rest, so the hold-and-wait condition deadlock
requires never arises. This is a structural property of the mechanism, not a policy.

**The orchestrator arbitrates blocks.** When a write set cannot be granted, the registry
returns every conflicting path together with its current holder and that holder's lease
expiry, and decides nothing itself. The orchestrator has three available responses: defer the
blocked work, narrow its scope, or cancel and re-dispatch the holder. All three are forms of
serializing the work, and each one taken is recorded.

**A worker never retries on its own.** It has neither the full roster of other workers nor the
overall plan, so a worker-side retry is a scheduling decision made by something that cannot
see the schedule, and a loop of such retries is a livelock. The registry exposes no retry
operation to a worker.

## Context

### Why prevention cannot be the orchestrator's job

An earlier option allowed either "ordered acquisition" or "lease timeouts with rollback."
Taking only the second would put the orchestrator in the business of breaking deadlocks by
preempting a worker that already holds a lease and is part-way through a write.

That is the worst available job for it. In measured multi-agent failure taxonomies, the two
largest failure categories are mismatched expectations between agents and broken commitments
between them, and preempting a committed worker mid-write is exactly such a failure by
construction: both sides had planned against a state that is being taken away from them. The
orchestrator's whole purpose is to reduce exactly those two failure categories by writing
briefs that keep expectations and commitments aligned; handing it a job that manufactures them
would work against that purpose.

So prevention has to be structural. Sorting a worker's write set and acquiring it atomically
is not a smarter policy decision; it removes the state a policy would otherwise have to
handle. Lease expiry stays only as a backstop for what ordering cannot reach: a worker that
has crashed or hung.

### Why arbitration cannot be the registry's job

A block is a scheduling question, and the registry has no visibility into scheduling.
Whether to defer, narrow, or preempt depends on the overall plan: what else is eligible to run,
how deep the current task breakdown is, whether the current holder is nearly finished, and
whether the blocked work sits on a critical path. Eligibility (whether a task's dependencies
are met) and admissibility (whether it collides with another worker's write set) are already
kept as separate questions, and admissibility is exactly the collision check above; judging
what to do about a failed admissibility check sits with the scheduler.

A lease wait renders as a visible edge on the graph interface from the waiting worker to the
current holder, alongside the other reasons a task is not progressing (dependency-blocked,
collision-deferred, concurrency-capped).

### Why every blocker is returned, not just the first

A caller told about only one blocking path at a time would acquire that path, discover the
next conflict, release, and repeat: a livelock dressed as progress that also perturbs the
lease table continuously for no benefit. Returning the whole conflict set lets the
orchestrator make one decision with complete information, which is the only kind of decision
worth making here.

Each conflict carries its holder's lease expiry, because the choice between deferring and
preempting depends on how long the wait is expected to last. A holder with seconds left and a
holder with minutes left are different situations, and an arbiter that cannot distinguish them
will get the choice wrong roughly half the time.

### Why an unforgeable token, not a checked path

The write-lease type has no public constructor; the only way to obtain one is a successful
acquisition that has already recorded the corresponding row. A file-writing operation that
requires this token cannot be called for an unleased path, so an unleased write does not
compile.

This mirrors the pattern already used for the project's own accounting ledger, for the same
underlying reason: a guarantee that matters must be enforced by the type system rather than by
a rule a worker is told to follow, since a system whose own workers edit its own code will
eventually produce a worker that breaks an unenforced rule for a locally sensible reason, and
the change will look reasonable on its own.

## Alternatives considered

**Timeouts and rollback alone.** Rejected as the primary mechanism, for the reason above: it
puts the orchestrator in the business of preempting committed workers, which manufactures the
two failure modes that dominate published multi-agent failure data. Retained only as the
backstop for crashes and hangs.

**Per-path acquisition with the caller looping.** Reintroduces hold-and-wait, which the atomic
whole-set acquisition exists to remove, and a looping caller is a livelock.

**Lock ordering advertised to workers as a convention.** Rejected on the same grounds as every
other unenforced convention in this project: it depends on a worker remembering to follow it,
and workers are exactly the thing being constrained.

**Sub-file range exclusion, built now.** Rejected for now and prepared for. A published
admission-control study found its dynamic-scope arm failing closed on undeclared write scope
in roughly half its executions, and most of those failures targeted files that were already
correctly declared but too coarsely, at whole-file granularity. Building finer-grained
exclusion before that granularity gap is actually measured on this project's own workload
would repeat the same prediction-before-measurement error found elsewhere in this project.
The declared write range is recorded from the first version of this mechanism so the data
exists when that decision is revisited.

**Registry prevents, orchestrator arbitrates.** Chosen.

## Consequences

Every grant is recorded, so the rate of paths held by more than one worker over time is
directly queryable, and the cost of this collision-prevention mechanism (how often it forces
serialization versus how often work would have collided anyway) can be measured against the
project's baseline rate of accidental file collisions.

Lease state sits on the same real-time change-log spine as everything else in the interface,
so the interface reflects an acquisition automatically, with no hand-placed notification, and
a future direct write to the lease table is reflected too.

An expired lease means the lease has gone stale, never that the worker holding it is dead; a
live worker renews its lease and keeps it, and a worker that returns after its lease has
lapsed finds its access token fenced and cannot write over whoever now holds the path.

Arbitrating a block well needs the holder's actual progress, not just its identity, which
requires periodic worker status reports; until those exist, the only honest arbitration policy
is to defer, which is also the cheapest.

**Reactivation trigger.** A measured serialization rate that lands near the maximum possible
rate rather than near the project's baseline accidental-collision rate, which would mean
file-level exclusion is too coarse in practice and would reopen the sub-file exclusion
question the declared write range was recorded to support.
