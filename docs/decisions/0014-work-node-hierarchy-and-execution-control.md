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

## Amendment (2026-10-14): a straight chain is one contract

Plans that were straight chains (each contract waiting on the one before) spent most of the run
in interface asks, handshakes and re-plans on edges that gained no parallelism: the planner's
time was 51 to 93 percent of the run window across 22 local runs. A split still costs an
interface to negotiate, a handshake, a merge, and a fresh worker that loses the previous step's
working state.

**The rule** (`plan_chain.rs`, applied in `orchestrator/chain_collapse.rs` before interfaces
are derived, for every plan revision including re-plans):

- A link `A -> B` is: `B` depends on `A` alone, and `A` has `B` as its only dependent. A chain
  is a maximal run of links, two contracts or more. The head may depend on contracts outside
  the chain, and the tail may have any dependents. This cannot make a cycle.
- The merged contract keeps the head's id. Instruction: one line saying it is one contract of N
  steps, then each step's instruction under `Step k of N (`id`):`. Acceptance: each step's
  acceptance under the same heading. Writes, slices and tools: the union. Dependencies: the
  head's; whatever depended on the tail depends on the merged contract. Attempts: the most any
  step asked for. Route: the first pin. Task kind: the steps' shared kind, else
  `implementation`.
- **One route, known before dispatch.** A chain merges only when every step runs on one route
  that is known now: the roster has one route (every routing policy), or every step pins the
  same route. With several routes and unpinned steps, rotation moves a cursor per dispatch and
  `fitness` picks by measured figures, so the steps could get different models; those chains
  stay split, so a mixed-model arm still measures mixing. The route must offer the union of the
  steps' tools.
- **One worker's size.** The merged contract is sized by the plan loop's own estimate (0057)
  against the route's plan target. A chain whose merged block is over the target is kept whole
  as the planner wrote it when a split could bring a step under the target; when the worker
  prefix and the projected growth alone are over the target, no split could make a step fit and
  the chain is merged. The children of a split are never merged back.
- **A plan as written stays.** Plan review (0094), an order the user chose (Scheduling above)
  and `OrchestratorConfig::collapse_chains = false` keep the plan as the planner wrote it. A
  plan the user approved up front is seeded, not decomposed, so it is never merged.
- **Loud.** One `chain_collapsed` trace record per merge (contract, merged ids, route,
  projected tokens, plan target) and a notice in the plan's notices. A chain kept as planned is
  a `notice` trace record with the reason.

The interface and handshake asks of the collapsed edges are not made. The cost is one worker
carrying more steps in one context, which the size rule bounds.

### Note (2026-10-14): a large code link stays split

A merged chain gives up the parallel start of decision 0081: the producer's interface stub
lands on the integration branch before any party is dispatched, so producer and consumer run
at the same time. A link `A -> B` of a chain is therefore kept split when both hold:

- **`A` can carry an interface.** `A`'s declared writes pass the interface-ask rule's own test
  (`derive_interface::unaskable_writes` is `None`: it declares a file, not only binary build
  outputs, and at least one source file in a language the checks compile or test). The same
  predicate decides which producers are asked for an interface. `B`'s writes do not matter.
- **`A` and `B` are each large.** Each step's own part of the work block is at least one
  quarter of the route's plan target (`PARALLEL_LINK_SHARE = 4`): the plan loop's base estimate
  (0057) of that step alone minus the worker prefix, that is its instruction and its slices or
  declared files' symbols. The prefix and the projected growth are the same for every block on
  the route, so they cannot tell a large step from a small one.

The chain is cut at every such link, and each run of two or more contracts on either side is
judged as any chain; a run of one stays as written. `a -> b -> c` with only `a -> b` qualifying
becomes `a` and a merged `b` that waits on `a`. A link whose route is not known before dispatch,
or whose size cannot be estimated, is left to the chain rules above.

**Why a quarter.** A parallel start saves at most the shorter step's worker time, and costs an
interface ask and a handshake first. Recorded interface asks took 75 to 186 s each on a local
model (fewer seconds on a hosted one), and the handshakes 81 to 178 s, while workers of small
steps in the same runs took 26 to 130 s. No per-route seconds-per-token figure exists to turn
a step's size into time, so the rule is in the sizer's units, set against the local cost; on a
hosted route it errs toward merging. Not measured: the saving of a parallel start on a large
code link.

**Loud.** A kept link is a `notice` trace record naming the link, the producer's checkable
files, both steps' own sizes and the plan target: `link A -> B kept split: A writes
interface-checkable code (...) and both steps are large enough to run in parallel against its
stub (...)`.
