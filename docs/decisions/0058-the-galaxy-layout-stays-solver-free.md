# Decision 0058: The galaxy layout stays solver-free; clusters grow by size bucket

**Status:** Accepted, from measurement. Amends decisions 0010 and 0015.
**Amends:** 0010 (the layout stays solver-free), 0015 (the layout stays solver-free)
**Related:** decisions 0007, 0054, 0055

This record exists so force-directed layout refinement is not re-proposed without new
evidence.

## Decision

The galaxy layout remains a pure function computed once and cached. No physics-based force
refinement runs, globally, per cluster, or on the graphics processor. An existing open-source
code-visualization tool's layout rules are ported: directory-hashed ring seeding, a vertical
axis driven by call depth, node color and size rules, and density-based visual intensity.
Cluster spread comes from a disc whose radius grows with the cluster's size in power-of-two
buckets, which matches the reference tool's visual spread for large clusters closely, without
running any solver.

**Amends the earlier layout decision:** an earlier plan for gentle force-directed refinement
against an anchor spring is withdrawn.

**Amends the earlier locality guarantee:** an earlier claim that refinement stays local "by
construction" does not hold for this algorithm. The exact cases in which an existing node
moves are: its cluster crossing a power-of-two size-bucket boundary above a threshold size (in
which case that cluster alone scales about its own center), and its own call depth changing.
Both kinds of movement reach the rendering layer through the normal change-log mechanism and
may be animated.

## Context

The visual target is a reference open-source code-visualization tool's look of a "nebula" of
clearly separated clusters. That look comes from force-directed refinement, which measurement
shows saturates at its own iteration limit and moves every node on any single insertion,
breaking the locality guarantee and, for insert-only variants, the caching guarantee as well.

## Alternatives considered

- **Global force-directed refinement, matching the reference tool.** Gives the best visual
  separation, but every insertion moves every node, and it is measurably slow at a moderate
  node count.
- **Per-cluster force-directed refinement.** Keeps node movement inside a single cluster, but
  still moves the whole cluster on every insertion and costs far more computation.
- **A fixed-size disc for every cluster.** Stable and cheap, but far tighter than the reference
  tool's spread, with more than half of stars visually overlapping.
- **Size-bucketed disc.** Chosen. Matches the reference tool's spread closely, with low
  overlap, is fast even at a large node count, and most insertions move nothing.

## Consequences

A planned GPU compute pass for force-directed refinement is no longer part of the rendering
build plan.

**Open question.** At a very large node count, several hundred clusters would crowd onto one
ring, causing significant visual overlap; a larger or multi-ring layout is needed and remains
to be measured.

Reactivated if a refinement approach is found that keeps the two-case locality guarantee
stated above.
