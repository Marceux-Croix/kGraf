# Decision 0018: Continuous merge, with quality gates as first-class work nodes

**Status:** Accepted. Its gate cadence clause is refined by decision 0019.
**Related:** decisions 0009, 0012, 0013, 0014, 0015, 0016, 0017

## Context

Weakening the item boundary (decision 0016) left no natural batching point
for merges. Continuous merge also surfaces conflicts one worker at a time,
small and attributable, rather than as one large merge at a boundary.

The risk continuous merge carries is that merges are unsupervised, and the
cross-agent textual conflict rate measured on real pull requests is 41.7
percent against 19.8 percent for same-agent pairs (decision 0009). A
cross-vendor design is exactly the applicable case.

Gates are the answer to that risk, and making them nodes rather than
background machinery means they inherit everything already decided. A
failing gate is a blocking node under decision 0014, with a visible
blocking edge to what it holds up. No new visual language, no new state
model.

### What gates do for the central design bet

The published evidence that most threatens this architecture concerns
semantic interference: changes that merge cleanly and then fail each
other's tests. That damage is invisible to git, which is why a
history-based measurement alone cannot catch it.

A gate catches exactly that class. It does not make the partition better,
but it makes an imperfect partition survivable rather than silently
corrupting. That materially reduces the cost of being wrong about the
central bet.

## Decision

**Worktrees merge back continuously**, as each worker finishes, not
batched at a boundary.

**Quality gates are work nodes in the graph**, inserted by the orchestrator
at intervals, not invisible infrastructure. A gate node has doneness, can
block, can be expanded, and competes for the concurrency budget like any
other node.

**A gate runs the project's declared validation commands**, discovered
from the document corpus under decision 0013.

**A gate runs against a pinned commit**, not the live trunk, so merging
never freezes.

**A failed gate halts new dispatch but does not cancel in-flight work.**
The remediation becomes its own work node.

## Consequences

**Gate cadence is a policy**, configurable under decision 0017, with
orchestrator discretion to insert an extra gate when risk is elevated, such
as after several cross-vendor merges land in the same region of the graph.

**There is an unverified window between gates**, and it must be named
rather than glossed. Work merged since the last passing gate is
unvalidated. Frequent gates shrink the window and cost more validation
runs; infrequent gates widen the blast radius. This tradeoff is exposed,
not hidden.

**A gate failure is attributable to a commit range**, since gates pin
commits. Whether the orchestrator bisects that range automatically is an
open question and is not decided here.

**New dispatch halts on failure; running work continues.** The halt is a
visible blocking state on the affected region of the graph, distinct from
a dependency block and from a concurrency-cap hold.

**A project with no discoverable validation commands gets no gate, and
kGraf says so.** Gates are discovered, never invented. The orchestrator
states plainly that it cannot gate quality and names what it would need,
rather than constructing a plausible-looking test command or silently
skipping the gate. This is the honest outcome for an untested repository
and the user should be told at ingestion, not at the first failure.

**This clarifies the verifier's primary role.** Decision 0009 introduced a
verifier to select among redundant parallel attempts, and decision 0012
made that mode secondary. The verifier's primary job is the periodic gate.

## Open

Whether the orchestrator bisects a failing commit range automatically, and
whether a gate failure should trigger a re-partition of the affected region
on the theory that the partition was wrong, are both open questions.

## Alternatives considered

**Batch merge at item boundaries.** Rejected: decision 0016 removed the
item boundary as a scheduling concept, and batching produces one large
conflict instead of several small attributable ones.

**Gates as invisible background jobs.** Rejected. An unseen gate is a
second system with its own state, and a failure would need a bespoke way to
surface. As nodes they are already visible, already blockable, already
expandable, and already counted against the concurrency cap.

**Gates run on the live trunk.** Rejected. Workers merge underneath a
running gate, so it tests a moving target and cannot say what passed.
Pinning a commit costs nothing and makes the result attributable.

**Freeze merging while a gate runs.** Rejected: it is serialization, which
decision 0012 already identifies as the trap that makes parallelism
nominal.

**Cancel in-flight workers on gate failure.** Rejected. Those workers may
have had nothing to do with the failure, and discarding their work to
punish someone else's breakage costs more than letting them finish into a
tree already known to be broken.

## Amendment (2026-09-30): a partial gate that cannot judge, and remediations that must fix

Every gate runs the project's validation commands, which cover the whole project
(`cargo test --workspace`). A gate below the root therefore also ran tests of contracts
outside its cover that were not built yet, failed on them, halted all other dispatch, and
spent its remediations on a failure no remediation in its region could fix; the contracts
that would have made it pass never started.

- **A gate below the root that fails is compared with the run's base commit.** The same
  commands run once per run at the integration tip recorded when the gates were planned,
  before any of the plan landed. If they fail there too, the gate is `inconclusive`: no
  remediation, no halt, nothing blocked, and its parent gates may run. The root gate covers
  everything and is never inconclusive, so the run completes only when the root gate passes
  or is waived. If the base passes, the failure is the cover's and the gate fails as before.
  A run with no recorded base (unversioned workspaces) judges its gates as they stand.
- **A remediation must pass the failing gate's commands.** It carries them, and the verifier
  runs them after the run's own check; the run's check alone can pass on the code the gate
  failed on.
- **A remediation that integrates no change counts as failed.** The gate opens the next
  remediation, whose brief says why the last did not count, or holds past the limit; it is
  never run again on the unchanged commit.
