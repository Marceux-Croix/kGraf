# Decision 0073: A merge conflict dispatches a resolver worker before a plain re-dispatch

**Status:** Accepted; amends decision 0018's consequence that a merge conflict is never
auto-resolved (0018 itself otherwise stands unchanged)
**Amends:** 0018 (merge conflict handling)
**Related:** decisions 0018, 0067

## Context

kGraf's orchestrator merges each worker's finished work back into the project's shared
integration branch continuously. Before this decision, a textual merge conflict simply
discarded the conflicting attempt's work outright and re-dispatched the same contract from the
new tip, from scratch. This throws away real work an agent already produced, purely because
two contracts happened to touch the same lines.

## Decision

**On a merge conflict, the orchestrator dispatches a resolver worker before it re-dispatches
the contract from scratch.** The resolver is an ordinary contract on the graph, with three
differences from a normal retry:

- Its declared write scope is limited to only the conflicted paths, and its brief carries the
  conflicting attempt's own change, the current integration tip, and the conflict's detail, all
  quoted as reference material rather than as an instruction to follow blindly. Its working
  copy starts fresh at the current tip, but the conflicting attempt's own commit stays
  reachable so the resolver can read it directly and bring the intended change in by hand.
- It shares the lease of the contract it is resolving, rather than taking out a new one, since
  the original contract is still open while the resolver works.
- It appears on the graph and in the session view marked distinctly as a resolver, and it
  cannot be addressed directly by the user the way an ordinary worker session can, since its
  entire job is this one merge.

**A resolver gets exactly one attempt.** If it succeeds, it closes the contract it was
resolving with its own result. If it fails for any reason (no verified attempt, a lost lease,
a further conflict), the run falls back to the original behavior: the contract is
re-dispatched from the new tip, and the resolver never retries itself.

## Consequences

- Kept, cross-vendor agent work is measurably more likely to produce a merge conflict than
  same-agent work, so this mechanism specifically addresses the case cross-vendor
  orchestration makes more common.
- The conflicted attempt's own commit is kept reachable, not discarded, until the resolver's
  outcome is known either way.
- The resolver's own merge, once it commits, goes through the exact same merge and
  verification path any other contract's attempt does; no new merge machinery was added.

## Alternatives considered

- **Keep the immediate re-dispatch, and only note the discard afterward.** Rejected: noting a
  discard does not recover the discarded work.
- **Give the resolver a fresh lease of its own.** Rejected: it deadlocked in testing, since the
  contract being resolved is still open and holding the same lease.
- **Resolve inside the original, conflicted working copy.** Rejected: that copy carries
  uncommitted intent from the first attempt, and would bias the resolution toward kGraf's own
  guess rather than a clean read of both sides.
- **Let a resolver retry itself if its own attempt also conflicts.** Rejected: a resolver
  resolving its own unresolved case is not the same guarantee; a plain fallback to the original
  contract is the more honest outcome.
