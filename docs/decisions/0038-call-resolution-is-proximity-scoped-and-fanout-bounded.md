# Decision 0038: Call resolution is name-based, proximity-scoped, and bounded in fan-out

**Status:** Accepted. A deliberate exception to decision 0020's "weight, do
not threshold" principle, argued once and in writing rather than
re-litigated every time someone reads the constant.
**Amended by:** 0065 (a call resolves only within its language family)
**Related:** decisions 0007, 0020

---

## Context

Decision 0020's general principle is that a value discarded at index time
cannot be recovered by a consumer that wanted a different operating point.
Applied literally to call resolution (matching a function call to the
definition it invokes), that principle says: write an edge to every
same-named definition and let the consumer filter.

A scale run on a 6,589-file, 1.39-million-line codebase measured exactly
that, and the measurement is the whole argument:

| | edges | wall clock |
|---|---|---|
| Edge per candidate | 6,847,862 | 124s |
| Proximity-scoped, fan-out bounded at 8 | 312,252 | 21.3s |

6.8 million edges on 100,769 nodes is roughly a hundred times the edge
density of a reference implementation's call graph, which was already
described elsewhere as larger than it needed to be. The cause is not
exotic: `new`, `from`, `len`, `push`, `get`, and `to_string` have thousands
of definitions each in a real repository, and every call site to one of them
was writing thousands of edges.

The key observation is that this is not a weaker answer than "unresolved."
It is the same answer, more expensively. An edge set stating that a call
might go to any of three thousand places tells a consumer nothing it did
not already know from the name. No operating point recovers information
from it, because there is no information in it to recover. That is what
distinguishes this case from the co-change case decision 0020 was written
about, where a single-observation pair genuinely is evidence and a threshold
genuinely destroys it.

Proximity scoping is a separate and unconditional improvement. It is the
cheap half of what a language server does, it costs nothing, and it is
usually right: in the same-file case it is right almost always.

The cost of the bound is stated rather than hidden. On the same checkout,
28,998 call sites exceeded it and 24,480 resolved to nothing in the index at
all, mostly calls into the standard library and third-party crates. The
call graph therefore reaches most but not all of the call sites in a real
repository.

That is acceptable because of what the call graph is now for. It was
already found not to work as a collision predictor, a job moved to
co-change, so what remains is navigation and building a context slice for a
worker, jobs where a missing edge costs a slightly thinner slice, not a
wrong answer.

## Decision

The code graph's `calls` edges are resolved by name, with no type
information.

Resolution is scoped by proximity: a definition in the caller's own file
wins; failing that, one in the same directory; failing that, the whole
index.

Where a name still matches more than eight definitions after scoping, the
call site is recorded as unresolved rather than as an edge to each
candidate. The count of call sites dropped this way is reported in the
index report.

Ambiguity that survives scoping and fits inside the bound is carried in the
edge weight, not discarded: an unambiguous resolution weighs 100, an n-way
one weighs 100/n.

## Consequences

The bound is a tractability limit, not an operating point, and must be
labeled as one. A consumer cannot recover a dropped call site from the
graph. If one ever needs to, the call sites themselves are stored in a
`call_sites` table and can be re-resolved under a different rule without
reparsing.

The dropped count is reported on every index. An exception to a standing
principle that is invisible in the output is an exception that will be
forgotten; the index report's ambiguity count is what stops that.

Eight is not a researched number. It is a starting point chosen to sit above
the common two- and three-way ambiguity and well below the pathological
cases. It is a candidate for future tuning, and it should be tuned against
context-slice quality rather than against edge count, since edge count is
the thing it trivially improves.

Storing call sites separately has a second benefit, which is why the table
exists at all: a reindex resolves names with a database query instead of
reparsing the tree, which is most of the speedup measured above.

This does not weaken decision 0020. Co-change keeps every pair with its
weight and thresholds nothing at index time. The exception here rests on the
specific claim that a maximally ambiguous call edge carries no information,
which is not true of a single-observation co-change pair, and the argument
does not generalize to any other relation without the same showing.

## Alternatives considered

**An edge per candidate, unbounded.** The literal reading of decision 0020.
Rejected on the measurement above: a hundredfold edge inflation, a 5.8x
indexing slowdown, and no recoverable information in the added edges.

**Resolve properly, with types.** Correct, and it is a language server.
Rejected for now on cost: it is a multi-month project per language, it needs
a build graph and a resolver per ecosystem, and the consumer of these edges
is a context slice that tolerates imprecision.

**Scope by proximity with no bound.** Better than unbounded, still not
enough. A ubiquitous name defined once per module in a large tree still
fans out widely from the global fallback, which is the case that produced
most of the 6.8 million edges.

**Drop ambiguous call sites entirely, with no bound and no weight.**
Rejected: it throws away the two- and three-way ambiguity that scoping
leaves, which is common, usually informative, and cheap to keep.

**Bound the fan-out, scope by proximity, weight what survives.** Chosen.

## Reactivation trigger

A measured finding that context slices are missing information a resolved
call graph would have supplied, or the arrival of a cheap per-language
resolver. Either reopens the "resolve properly" option, which is the real
fix.
