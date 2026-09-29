# Decision 0016: The scheduler's scope is the whole work graph, not one item

**Status:** Accepted
**Related:** decisions 0012, 0014, 0015

## Context

Making work a recursive hierarchy (decision 0014) raised the question of
whether the hierarchy is also the scheduling unit. It is not. Items are an
organizational level, not a barrier.

The alternative was to schedule one item at a time, which bounds the
partition search space to one parent's children. That is a tractability
argument, and it was declined: it would leave obvious parallelism unused
whenever an item's children are few or serially dependent, and it would
make the plan's organizational structure silently determine throughput.

## Decision

There is no scheduling boundary at the plan-item level. A work node becomes
eligible when its dependencies are satisfied, regardless of which parent it
hangs under, and the scheduler selects a maximal non-colliding set from
everything eligible.

Parallelism therefore runs both within an item, across sibling sub-nodes,
and across items, between nodes under different parents.

## Consequences

**The partition candidate set is the whole eligible frontier.** Combined
with decision 0015's continuous partitioning, this means recomputing a
non-colliding set over an unbounded candidate set as the plan changes. Cost
and latency of that computation are an implementation concern.

**Eligibility and collision are separate checks.** A node is eligible when
its dependencies are met; it is admissible when its predicted write set
does not collide with a running worker. Both must pass. Conflating them
would make a blocked node and a colliding node indistinguishable, and
decision 0014 requires the interface to show which is which.

**Concurrency limits stop being implicit.** With an item boundary, the
number of in-flight workers was bounded by an item's width. Without one,
the frontier can be arbitrarily wide, so a concurrency cap becomes an
explicit setting rather than an emergent property. See decision 0017.

**Reactivation trigger:** partition computation over the full frontier
proving too slow or too expensive to run continuously. The response would
be to bound the candidate set by some heuristic, which may or may not be
the item boundary, and would supersede this record.
