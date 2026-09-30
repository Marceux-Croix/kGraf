# Decision 0020: Co-change is the collision relation, computed by kGraf, weighted and never thresholded

**Status:** Accepted
**Related:** decisions 0007, 0009, 0012, 0016, 0018

## Context

Measurement found that a call graph's disjointness verdict is close to
statistically independent of whether two changes collide. The diagnosis was
that 36 to 82 percent of the files involved in a missed collision are
unreachable from the seed within four hops, and that repositories
constantly change files a code indexer does not model at all.

The underlying reason is that a call graph and a collision predictor
answer different questions. A call graph encodes what the code is,
derivable from a single snapshot. Collision risk depends on what changes
with what, derivable only from history.

### The measurement that decides the threshold question

A reference co-change implementation, indexed on a repository of 2,402
files over 2,076 commits, produced 219 edges covering 121 files, five
percent of the repository, against 68,259 call edges.

Recomputing the same relation directly from that repository's git log, in
about two seconds:

| Filter | pairs | file coverage |
|---|---|---|
| Every observed pair | 19,341 | 99% |
| co-change >= 2 | 3,350 | 45% |
| co-change >= 3 | 1,422 | 26% |
| co-change >= 3 and coupling >= 0.3 | 202 | 14% |

The reference implementation's default thresholds discard 99.96 percent of
the relation, and its coupling cut alone removes 93 percent of what
survived the co-change cut.

Those thresholds are not wrong; they are tuned for a different job. A tool
that tells a developer "you changed this, do not forget that" wants
precision, because a false hint is annoying and a missed hint is invisible.
Collision risk wants the opposite: if there is any history of two files
moving together, that is worth knowing.

The unthresholded relation is also entirely tractable. 19,341 edges is 3.5
times smaller than the call graph already carried.

### Why kGraf's own observation beats git

Git history structurally cannot show a collision. By the time work is
committed a human has already resolved the conflict, so any figure derived
from git history is a floor on the true collision rate.

Watching every write of every worker, per task, as it happens gives
directly observed, labeled collision events, which is ground truth git can
never contain. The observation is also finer: task-level write sets rather
than squashed commits, including work later reverted or discarded, which is
precisely what review strips out of history.

## Decision

**The code graph is not the collision predictor.** It remains the context
source and the substrate of the interface, which are the jobs it is good
at.

**Co-change is the collision relation**, and kGraf computes it itself from
version history rather than consuming another tool's version of it.

**Every observed pair is kept, with its co-change count as a weight.
Nothing is thresholded at index time.** The consumer picks its own
operating point.

**Sources are unioned**: git commits regardless of who or what authored
them, plus kGraf's own observed per-task write sets, which include observed
collisions.

## Consequences

**Weight, do not threshold.** This is the general principle and it
outlives this record. A value discarded at index time cannot be recovered
by a consumer that wanted a different operating point.

**Refinements the reference implementation does not do**, and which the
data already argues for:

- **Recency weighting.** A pair that co-changed last week predicts more
  than one that co-changed two years ago.
- **Commit-size filtering.** Median commit size in the sampled repository
  is 3 files and the maximum is 713. Mega-commits create spurious pairs
  across the whole tree, and the measured pair counts move by an order of
  magnitude depending on the cap.
- **Sub-file granularity**, eventually. File level is where this starts.

**Sources union, and degrade gracefully.** Git is read for everything
regardless of authorship, so a user switching between kGraf and another
harness loses task attribution and collision observations for that window
but not the file-level relation. The model must not assume it owns the
history.

**The call graph keeps its jobs.** Context slices, navigation, and the
visual structure of the graph. It is being removed from one role, not
demoted generally.

**This supersedes any earlier claim that call-edge confidence is
load-bearing for collision-avoidance correctness.** That claim is
falsified: 36 to 82 percent of missed files have no path from the seed at
all, confidence thresholding does not move the frontier, and the
high-confidence configuration collapses onto the naive baseline to three
decimal places. The failure is missing structure, not uncertain structure.

**Cost is negligible.** Computing the relation over 2,076 commits took
about two seconds. This is not a component that needs its own performance
budget.

## Open

Whether co-change is a sufficient collision predictor is not settled by
this record. It is the right relation; whether it is accurate enough to
gate parallel dispatch is a further measurement question. If it also
fails, the fallback remains sequential specialization.

## Alternatives considered

**Keep using the reference implementation's co-change edges.** Rejected.
Its operating point is chosen for another purpose, and a threshold someone
else picked would silently govern whether two agents collide.

**Loosen the reference implementation's thresholds.** Better, and still
rejected. It concedes the principle that a binary keep-or-drop decision
belongs at index time.

**Threshold at index time with kGraf's own cutoff.** Rejected for the same
reason. The correct operating point is not knowable at index time, depends
on the consumer, and is exactly what a probe should sweep. Discarding the
weight makes that sweep impossible.

**Compute it in kGraf, keep every pair, carry the weight.** Chosen.

## Amendment (2026-09-29): observed write sets and collisions are recorded

The orchestrator now writes what the decision names as a source. When an attempt
finishes, its changed paths are recorded as that contract's write set, for every
attempt: the one merged, the ones that lost, and the ones that failed. When a
contract's merge into the integration branch conflicts on a path that another
contract of the same run already merged, one collision per pair and path is
recorded with kind `textual`. A conflict on a path no other contract of the run
wrote has no second task, so nothing is recorded for it. `kgraf cochange` folds the
write sets into the relation and reports the collision count.
