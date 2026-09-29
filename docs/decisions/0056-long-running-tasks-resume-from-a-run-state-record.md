# Decision 0056: Long-running tasks resume from a run-state record, and the graph is the memory

**Status:** Accepted
**Related:** decisions 0015, 0026, 0028, 0030, 0036, 0043, 0052, 0055

This record exists so the long-running task design is not re-argued. If a contributor asks
why kGraf has no separate memory system, or how a multi-day run survives a restart, the
reasoning is here.

## Decision

Users can run tasks that last hours to days across many separate sessions. Four mechanisms
support this, each taken from existing published work where possible.

1. **A run-state record.** The orchestrator keeps a machine-readable record of each run: open
   contracts, admitted write leases, the last verified commit, and the next planned dispatch.
   On restart it reads this record before it re-plans. The record lives in the graph store
   itself, not in a second surface. This follows a published pattern for long-running agent
   harnesses that combines a progress record with a git checkpoint. Durable step replay is
   evaluated against an existing SQLite-backed workflow library before any custom engine is
   written.
2. **A capped private context, reset through compaction.** A worker's private context is
   capped and periodically reduced, because measured quality degrades well below the nominal
   context limit of most models.
3. **Stall detection.** The agent loop detects repeated identical tool calls inside its step
   budget. No existing permissively licensed library provides this, so it is built new, kept
   small.
4. **The graph is the memory.** kGraf does not add a separate vector-store or summary-based
   memory system. Agent conversations and decisions are durable graph nodes that are never
   pruned, and retrieval is structural rather than semantic search over a separate store.
   Where the graph itself lacks a notion of temporal validity (when a fact stopped being
   true), an existing open-source graph-memory design is ported for that purpose.

Two further items are planned as measured, rather than assumed, improvements: workflow
templates mined from successful task breakdowns, and measuring whether making long tasks
easier to run increases how many get run, since the population of long tasks is expected to
grow as models improve. Acceptance for the orchestrator work also adds a published checklist
of multi-agent failure modes.

## Context

Long-running tasks were identified as a first-class use of the product, on the reasoning that
pre-planning and coordination make long tasks feasible, and that they demand maximum
efficiency. Much of the existing design already covered this; a review of the published
literature confirmed those choices and exposed four gaps: restart state for the orchestrator
itself, degradation inside a worker's context over time, stall detection, and an unstated
position on memory.

## Alternatives considered

- **A separate memory system**, of the kind used by several existing agent memory products.
  Their reported gains are largely single-paper and self-reported results, and such a system
  would duplicate what the graph already holds. Their consolidation ideas are ported only
  where the graph genuinely lacks them.
- **A workflow engine with a server component.** Needs a standalone server process, which
  conflicts with a local, single-user app. One considered option is also under a
  source-available license that does not qualify for reuse.
- **Resume from a single worker's own transcript alone.** Covers one worker's own continuity,
  not the orchestrator's overall plan. Kept as the mechanism for an individual worker; the
  run-state record covers the orchestrator.

## Consequences

Early work gains the context cap and stall detection. Later orchestrator work gains the
run-state record, restart acceptance criteria, and the failure-mode checklist. Later
measurement work gains workflow templates and the induced-demand measurement described above.

No savings claim follows from any of this.

Reactivated if measurement shows that structural retrieval from the graph misses information
that a semantic memory system would find in long runs.
