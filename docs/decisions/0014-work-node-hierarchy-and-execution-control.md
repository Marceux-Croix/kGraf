# Decision 0014: Recursive work nodes, derived progress and blocking, and two execution modes

**Status:** Accepted
**Amended by:** 0015 (expansion behavior), 0025 (the recursion floor)
**Related:** decisions 0003, 0004, 0007, 0010, 0012, 0013

## Context

Removing the kanban board (decision 0003) accepted a debt: every state a
board would have shown must find an expression on the graph, and some would
be genuinely hard. This record pays that debt.

Hierarchy, progress, and blocking are exactly the information a board
conveys through columns, card state, and swimlanes. Expressed on the graph
they cost no second surface, stay in one place, and cannot drift from the
work they describe.

Sequencing is, by default, the orchestrator's job. Permitting a
user-chosen order is an override on a default the orchestrator otherwise
owns, following the same pattern already established for model pinning
(decision 0004): it is not a separate mechanism and not a replacement.
Selecting a node on the graph and pressing play is not the second surface
the no-kanban rule exists to prevent, because it stays on the graph.

## Decision

**Work is a recursive node hierarchy.** A part decomposes into sub-nodes,
which decompose further, until a part fits one worker's brief. Work nodes
are a distinct class from code nodes and agent nodes.

**Clicking a work node expands it**, revealing its children in place.

**Every work node carries two derived indicators**: doneness, and what is
blocking it. Blocking is a first-class edge with its own visual channel.

**Two execution modes, mixable.** A play control hands scheduling to the
orchestrator entirely. Alternatively the user selects nodes and an order,
and the orchestrator works that order. Pinning some nodes while the
orchestrator fills around them is the expected middle case.

**User-selected order does not override write-scope admission.** Priority
is the user's; safety remains the orchestrator's.

## Consequences

### Graph schema

Work nodes are a distinct label class from code nodes and agent nodes, with
a containment edge expressing decomposition, following the same pattern
already used for files and folders.

A blocking edge type is added. It is derived and rewritten as facts change,
not authored.

Work nodes link to the code nodes they are expected to create or modify.
That link is the predicted write set from decision 0012, so the partition
and the interface are reading the same structure.

### Rendering

**Children are laid out with their parent, once, and never re-solved.**
Child positions cluster tightly around the parent's anchor and are computed
in the same server-side pass as everything else. Expansion changes
visibility and moves the camera; it does not re-solve the layout.

**Three node classes now carry three click behaviors**: a work node
expands, a code node highlights its neighborhood, an agent highlights its
live edges and opens its transcript. This is learnable but must be visually
obvious, since the same gesture does three things.

**Doneness and blocking need visual channels that are not already taken.**
Degree owns color, label class owns size, and agents own their own geometry
(decision 0010). Progress and blocked state need a fourth and fifth channel
that survive the density rules at scale.

**Per-node UI elements render only on work nodes**, which are bounded by
plan size rather than codebase size. Drawing progress indicators on ten
thousand code nodes is not viable and is not required.

### Derivation

**Doneness** aggregates over children; a leaf's doneness comes from its
worker's reported state. Both are derived from durable facts (decision
0007).

**Blocking** is derived from an unfinished dependency, a denied write
scope, or a worker reporting stuck or needs-input. Because it is derived, a
node becomes visibly blocked the moment the cause exists, with nothing to
remember to set.

### Decomposition

**Recursion needs a floor.** Decomposition stops when a part fits one
worker's brief. Without a stated floor an orchestrator can decompose
indefinitely, and each level costs a model call.

**Decomposition is itself work the user can see and correct.** It appears
on the graph as it happens rather than resolving invisibly.

### Scheduling

The orchestrator's scheduler must accept a partial order from the user and
complete it, rather than choosing between full autonomy and full manual
control.

**Admission control binds in both modes.** A user-queued pair that
collides is run in sequence, and the orchestrator says why. This keeps user
priority and system safety separable.

## Open

How deep decomposition should go before a part is considered brief-sized is
not settled. Too shallow wastes the partition; too deep spends model calls
on decomposition that could have been work.

## Alternatives considered

**Flat work items, no hierarchy.** Rejected: a roadmap item is rarely one
worker's brief, and decision 0012 makes decomposition quality the project's
core competence. Decomposition has to be visible to be trusted.

**Hierarchy shown in a side panel or tree view.** Rejected. It is a second
place to look, and it would drift from the graph.

**Expansion via re-layout.** Rejected. Decision 0010 caches positions
precisely so nodes never teleport between reindexes; a re-solve on every
expansion discards that guarantee for the most frequent interaction in the
product.

**Stored progress and blocked flags.** Rejected under decision 0007. Stored
status is a second copy of the truth and will drift. Derivation also makes
blocking automatic rather than something a worker must remember to
declare.

**User order overriding admission control.** Rejected. It would let the
user request a collision, and the resulting conflict would be
indistinguishable from a partition failure, corrupting the measurement of
how well the design partitions work.
