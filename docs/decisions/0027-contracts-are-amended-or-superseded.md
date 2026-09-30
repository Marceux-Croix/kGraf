# Decision 0027: An inadequate contract is marked, then amended or superseded, never silently edited

**Status:** Accepted. Closes the open question in decision 0026 on contract
revision mid-flight.
**Related:** decisions 0003, 0009, 0014, 0018, 0024, 0026

---

## Context

Decision 0026 made an interface contract a committed artifact: a signature
and a test written before either worker starts. It left unresolved what
happens when that contract proves inadequate while workers are already
building against it.

The answer applies the same discipline the project's own decision records
use: a reversed decision gets a new superseding record rather than an edit,
so the reasoning survives. A contract is an artifact of the same kind, and
silently correcting one erases the fact that work was done against a
different understanding.

The property that matters is that inadequacy becomes visible rather than
absorbed. A worker that already delivered against the old contract has a
recorded reason its output is now stale, instead of discovering it at
integration or not at all.

### Amend versus supersede needs a stated test

Without one, the distinction collapses under pressure toward whichever is
less disruptive.

Amend when the change is additive and work built against the prior version
still holds: widening a type, adding an optional parameter, adding a field.
Existing deliveries remain valid.

Supersede when the change breaks: narrowing a type, changing a signature,
altering expected behavior. Existing deliveries are now wrong.

The test is a question about the contract, not about how expensive the
answer is. A breaking change called an amendment because supersession is
costly is the failure this record exists to prevent.

### Who detects it, and why it needs its own report kind

The cheapest detector is the worker implementing against the contract, which
is earlier than a gate catching the mismatch at integration.

None of the existing report kinds fit: informational batches, a
needs-input concern that only the sender cares about, a stuck report meaning
the sender needs help, and a done report that settles the work. Contract
inadequacy is different in a way that matters: it affects other workers, so
it has blast radius and must fan out rather than route only to the
orchestrator.

It is therefore a fifth report kind, delivered immediately and fanned out to
every agent holding the affected contract, alongside the orchestrator.

## Decision

A contract that proves inadequate is marked inadequate in its own artifact,
and the record of its inadequacy persists. It is never silently corrected.

Amend when the change is additive and work already built against the
contract still holds. Supersede when it breaks, meaning delivered work is now
wrong. A superseding contract names the one it replaces; the replaced one
names its successor.

Discovery of an inadequate contract is a distinct report kind with fan-out
semantics.

A superseded contract blocks every node built against it, visibly, until the
orchestrator resolves each one.

## Consequences

Contract artifacts carry status: active, amended, or superseded, with a
pointer to the successor where one exists. Status is derived from the
artifact itself.

A superseded contract creates blocking edges to every node built against it,
reusing existing graph machinery. No new visual language is needed: it is a
blocking node with an edge to what it holds up, and the reason is stated.

The orchestrator resolves each blocked node individually. Some delivered work
may survive a supersession and some may not, and treating them uniformly
would discard valid output.

The report policy grows to five kinds, and the fifth is the only one that
fans out to peers rather than routing to the orchestrator alone.

This is the project's documentation discipline becoming its runtime
discipline, which is a coherence property worth keeping: the same rule
governs a decision record and a live interface, so a contributor already
knows how one behaves from the other.

## Open questions

Whether an amendment should still notify holders is unsettled. It does not
invalidate their work, but a worker unaware of a widened type will not use
it. The cost of notifying is a message; the cost of not notifying is
silently unused capability.

## Alternatives considered

**Silently correct the contract and re-dispatch.** Rejected. It destroys the
record that work was done under a different understanding, and a worker
whose delivery is invalidated has no way to learn why.

**Treat every revision as a supersession.** Rejected as too costly. Additive
changes genuinely do not invalidate prior work, and forcing a full
supersession would discard valid deliveries.

**Treat it as an ordinary stuck report.** Rejected. A stuck report routes to
the orchestrator for help with one worker's problem. A bad contract is a
shared problem and must reach everyone holding it.

**Let the gate catch it.** Retained as a backstop, rejected as the primary
path. The gate is the latest possible detection point, after both workers
have built.

## Amendment: a work contract's inadequacy, reported by its worker (decision 0093)

A worker whose instructions are missing something or wrong reports it with `report_shortfall`
(kind `instructions`), on any contract. The orchestrator marks it on the contract, never as a
silent edit, and amends additively once: the next attempt's brief quotes the report as data. A
second report holds for the user. A work contract has no peers holding it, so the report goes to
the orchestrator alone.
