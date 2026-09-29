# Decision 0015: The graph is dynamic by design; stability means local perturbation

**Status:** Accepted. Refines decision 0010's stability property and
decision 0014's expansion behavior. Neither is reversed.
**Related:** decisions 0007, 0010, 0012, 0014

## Context

The real requirement was never global stasis. Decision 0010 wanted
positional stability so that agents would not appear to teleport between
reindexes, which is about spurious motion, not about motion as such. Motion
caused by the work is the product; motion caused by unrelated work is the
defect.

### The anchored layout already provides this

Each node's anchor is seeded deterministically from a hash of its own
identity, not from a global force solve. Inserting a node therefore does
not move its neighbors, because their anchors are functions of their own
names. The refinement pass is gentle, anchored by a spring at a fraction of
node mass, and displacement-capped per step, so a perturbation stays local
by construction.

This property was recorded in decision 0010 as stability across reindexes.
It is the same mechanism that makes the dynamic case work.

## Decision

**The graph changes continuously during development and is expected to.**
Work nodes are created by decomposition, completed, superseded, and
abandoned while the user watches. A static graph would be a graph not
showing the work.

The stability property is local perturbation, not global stasis: inserting
or removing a node must not move nodes elsewhere in the scene.

**Completed work collapses into its parent rather than vanishing**, and
remains expandable.

**Node appearance and removal are animated in place**, never popped.

**Partitioning is continuous, not once per item.** Because decomposition is
ongoing, a new sub-node can appear whose predicted write set collides with
a worker already running.

## Consequences

**The stability test is not that nothing moves. It is that nodes unrelated
to a change do not move**, measured by comparing positions before and after
an insertion in a distant part of the graph.

**Layout must assign a position to a node that did not exist at index
time.** Because anchors derive from identity, this is a pure function and
needs no re-solve. A work node's anchor derives from its identity and its
parent's anchor, so children cluster without being solved against the whole
scene.

**Removal must not leave a hole that pulls the layout.** Since positions
are anchored rather than emergent, removing a node does not release tension
into its neighbors.

**Collapse is the default for completed children**, with the parent's
derived doneness standing in for them. The expansion interaction from
decision 0014 already supplies the way back in.

**Animation is a requirement, not polish.** In a scene where nodes
legitimately appear and disappear, an un-animated change is
indistinguishable from a rendering fault. Appearance and removal are
transitions.

**Partitioning becomes continuous, and this is the sharp consequence.**
Decision 0012 assumed a partition computed per item before workers spawn.
With ongoing decomposition, a sub-node can appear mid-flight whose
predicted write set collides with a running worker.

That case is strictly harder than the static one, because the colliding
worker has already begun. The available responses are to defer the new
sub-node until the running worker finishes, to narrow its brief to the
non-colliding remainder, or to cancel and re-dispatch the running worker.
All three are forms of serialization, and each one taken is an execution
that the design's serialization measurement must count.

**The central question extends.** It is not only whether the graph can
partition work correctly in advance, but whether it can do so
incrementally, as the plan itself changes. The static case is a necessary
condition and is testable from history; the dynamic case cannot be tested
that way and must be measured as it happens.

## Alternatives considered

**Global re-solve on change.** Rejected: it reintroduces exactly the
teleporting that decision 0010 exists to prevent, and it does so on the
most frequent event in the system.

**Freeze the layout entirely after first solve.** Rejected: new nodes would
have nowhere principled to go, and the graph would stop reflecting the
work.

**Delete completed work nodes.** Rejected. It discards history and would
make a finished parent's decomposition unreviewable.

**Keep completed children rendered permanently.** Rejected as noise at
scale.

**Local perturbation with collapse-on-completion.** Chosen.
