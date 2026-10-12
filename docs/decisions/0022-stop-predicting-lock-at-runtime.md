# Decision 0022: Stop predicting collisions. Lock at runtime, prefer dissimilar tasks, gate for the rest

**Status:** Accepted. A figure in this record is corrected by decision 0023: the
claim below that "33 percent of textually clean merges had build or test
conflicts" is wrong. The correct figures are 33.3 percent of all conflicts and
9.3 percent of clean merges. The source paper states its own finding
incorrectly and the error propagated here.
**Amended by:** 0023 (corrects a figure), 0049 (the acceptance criterion)
**Related:** decisions 0009, 0012, 0016, 0017, 0018, 0019, 0020, 0021, 0023

---

## Context

### Why prediction is abandoned

kGraf does not predict which files a task will touch. Measurement and the
published literature agree that this does not work well enough to gate
dispatch.

A call-graph based predictor of which files change together was measured
against a random file set and barely beat it: lift 0.73 to 1.16, within 0.062
of a random baseline at every operating point, best F1 0.127.

Hassan and Holt (ICSM 2004) ran the same comparison and found call, use, and
define edges reach recall 0.42, leaving 58 percent of needed co-changes
unreachable. Their conclusion: results "cast doubt on the effectiveness of
code structures such as call graphs as good indicators for change
propagation."

Co-change (files that changed together historically) is the right relation
but not a sufficient predictor. Raw co-change scores F1 0.112, below the
call-graph result already ruled out above. A pruned hybrid that reaches F1
0.50 is the literature's ceiling, and 0.50 is not a figure to gate unattended
dispatch on.

Sparsity is the binding constraint, not accuracy: for one open-source
project's 1.78 million class pairs, logical coupling produced information for
about 180 of them.

### Why a hot-file lock set is abandoned specifically

A per-file lease (a worker acquires exclusive write access to a file before
editing it) has no useful "hot set" to optimize. For a per-file lease, base
collision rate equals residual collisions plus serialized collisions
identically by construction: a lease fires when and only when two agents want
the same file, which is exactly the condition for a textual collision. Every
prevented collision is a serialized pair. There is nothing to tune.

The hot set is also dominated. Efficiency, meaning collisions removed per pair
serialized, measured 0.67 to 0.78 for hot sets of 10 to 100 files, and 1.00
for leasing every file. Locking everything is strictly better than locking a
busy subset.

Drift confirms it independently: in one measured repository, a hot set's
coverage fell to 0.000 within five days, and recomputing it did not help
because work had moved to modules that had not previously changed. That is a
regime switch, not decay.

A random lock-set control passed, so hotness is a real effect. It does not
matter, because a simpler mechanism dominates it.

### What locking everything actually costs

Under a per-file lease with no hot set, residual textual collisions measured
0.000, at a serialization rate equal to the base collision rate: 0.316 and
0.243 in two measured repositories.

So 68 to 76 percent of pairs still ran concurrently, with zero textual
collisions. That is real parallelism obtained without predicting anything.

This reframes the design. Prediction was only necessary because partitioning
was assumed to happen in advance. A runtime lease does not predict; it blocks
when contention actually occurs, and pays serialization only where contention
is real rather than where a model guessed it might be.

### The task-similarity result

Commit pairs sharing a significant subject term collide 2.25 times (in a
Python codebase) and 2.28 times (in a Rust codebase) as often as unrelated
pairs, with non-overlapping confidence intervals, replicated across both
languages. Conventional-commit scope (a structured commit prefix naming the
affected area) gives 2.48.

A path-based directory proxy (assuming tasks in the same directory conflict)
gives the weakest effect measured, 1.21 with overlapping intervals in the
Python codebase, the opposite of what a circularity concern would predict.
Path-blind proxies, based on shared vocabulary rather than shared paths, are
the strong ones. Removing shared terms that appear in either commit's own
file paths halves the Python effect to 1.72 but leaves the Rust effect at
2.25.

In the same measurement vocabulary as the call-graph result above: lift 0.74
at yield 0.71 in both languages, against the graph's best cell of lift 0.74 at
yield 0.137 in one language that did not survive a robustness check.

It is not a safety mechanism: contamination measured 0.233 and 0.181 against a
0.10 bar for a safety-grade signal. It is a scheduling preference.

It is not redundant with the lease: after a 20-file lease, the same-versus-cross
residual ratio rises to 4.01 and 2.70. Stacked, out of sample, they give a
residual collision rate of 0.079 and 0.106, the best combination measured.

And it confirms an inverted default: sibling sub-tasks (parts of the same
feature) are the worst pairs to run concurrently, not the best.

### Speculative execution addresses what locking cannot see

Runtime leasing, similarity scheduling, and a lease-based lock prevent textual
collisions but leave semantic collisions unaddressed: two changes to
different files that merge cleanly and then fail each other's tests. The base
rate for that class is large, corrected in decision 0023 to 9.3 percent of
textually clean merges.

Systems that work in this space do not predict; they observe. One published
system achieves precision 1.0 by construction because it performs the merge,
build, and test rather than estimating whether they would succeed, rather
than reporting current concurrent-edit state from static analysis.

kGraf is structurally positioned for this: every writing worker already gets
an isolated git worktree, the harness already discovers the project's
validation commands, and agent work is cheap to re-run. The pieces needed for
speculative merging already exist for other reasons, so the marginal cost is
scheduling the runs, not building new machinery.

It also answers an objection that merged history hides conflicts. One study
could not read conflicts off mainline history, so it reconstructed 550,000
development versions and speculatively merged them. It found that 19 percent
of potential merges would have produced a textual conflict that never
appears as a conflict in the repository. That is direct evidence that a
measurement taken from merged history is a floor, not a ceiling, and it names
the only known way to see past it: do not mine merged mainline history,
speculatively merge parallel states and observe what actually breaks.

Cadence is an open question. Continuous background speculation across every
in-flight pair is the strongest signal and the largest compute cost; running
it only at checkpoints is the cheapest and slowest to detect. The cost is
compute already being paid for in test runs, not additional model spend.

## Decision

kGraf does not predict which files a task will touch. Four mechanisms replace
prediction, each doing a job it can actually do:

1. **Runtime per-file mutual exclusion.** A worker acquires a lease on a file
   when it writes it. This is exact, not probabilistic. No hot set, no
   ranking, no recomputation.
2. **Task-similarity as a scheduling preference.** Tasks whose descriptions
   share significant vocabulary are deprioritized for concurrent dispatch.
3. **Speculative merge, build, and test** across in-flight worker branches,
   running continuously in the background. This is ground truth, not
   prediction.
4. **Gates**, checkpoints where speculative results become blocking, as the
   place semantic conflicts are finally caught.

## Consequences

Locking is exact, so the gate's job narrows to semantics: two changes to
different files that merge cleanly and then fail each other's tests. Gates
are the only mechanism that addresses this, which makes them load-bearing
rather than merely good practice.

Serialization is now a measured cost, not a risk. Expect roughly a quarter to
a third of candidate pairs to serialize on contention, near the repository's
base collision rate.

The scheduler prefers cross-feature pairs over sibling pairs, reversing the
intuitive default. Implementation is a set intersection over task text with no
runtime coordination, close to free.

Task descriptions become load-bearing input. The signal is text similarity
between task descriptions, so decomposition quality now affects throughput
directly. A decomposition that produces vaguely worded sibling tasks loses the
signal.

Lease waits must be visible to the user: a worker blocked on a lease is a
distinct reason a node is not progressing, alongside dependency-blocked,
collision-deferred, and concurrency-capped.

Deadlock is now possible and was not before. Two workers each holding a file
the other wants is a real state. Ordered acquisition, or lease timeouts with
rollback, is a requirement going forward.

Co-change is no longer the collision predictor, but weighting a derived
relation rather than thresholding it remains correct in general, and
reference-implementation thresholds were found to discard 99.96 percent of
that relation. Co-change may still earn a place as a secondary scheduling
signal; it is no longer the mechanism.

Runtime locking needs no history, so a directory-proximity prior for
cold-start scheduling is no longer needed for safety. It may still inform
scheduling preference before task text accumulates.

## The caveat that outranks every number above

All of the numbers above are textual overlap measured in merged history.
Semantic coupling between different files is structurally invisible to every
experiment run so far, and the published studies found most of their damage
there.

The residual collision rate of 0.079 is a residual *textual* rate. The
semantic residual is unmeasured and unbounded.

Resolving it requires a build-and-test replay of the same pairs, which is
more expensive than anything run so far because it requires executing each
historical build. Speculative execution is the mitigation that does not need
that replay to happen first, because it observes rather than estimates: it
reduces the semantic residual regardless of how large it turns out to be.

## Alternatives considered

**Continue to a co-change predictor.** Rejected. Raw co-change is worse than
the already-rejected call graph, the pruned ceiling is F1 0.50, and sparsity
means it abstains on nearly everything.

**Hot-file leasing.** Rejected as dominated by leasing every file, on a
measured efficiency comparison, with a five-day drift failure in one
repository.

**A coarse global lock.** Rejected: that is full serialization, and per-file
leasing achieves zero textual collisions at 24 to 32 percent serialization
instead of 100.

**Speculative merge and test as the only mechanism, with no locking.**
Rejected as insufficient alone: it detects rather than prevents, so two
workers would still corrupt each other's trees and speculation would only
report the damage afterward. Locking prevents the textual class cheaply;
speculation catches what locking cannot see. They are complementary, not
alternatives.

**Build a conflict-minimizing scheduler.** Rejected as solving the wrong
half. One published system showed constraint solving finds conflict-minimal
orderings in seconds, but took its file sets from actual historical
changesets, which is knowledge not available in advance. Its own future work
names automating that as the open problem. Prediction is the hard part and
scheduling is a solved component if it is ever needed.

## Amendment (2026-09-29): the lease measurements are reported

The run report and the `kgraf orchestrate` report now carry the run's lease
measurements: the paths its agents held one after another (the serialization the
decision calls a measured cost) and the share of those handoffs, where both grants
declared a range, whose ranges did not overlap. The share is not reported when no
handoff had ranges on both sides, rather than shown as zero. Both are counted over
the run's own grants only.
