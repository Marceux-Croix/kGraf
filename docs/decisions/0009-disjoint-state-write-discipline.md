# Decision 0009: Workers read widely, write disjointly, and are selected by a verifier

**Status:** Accepted. Its verifier-selection clause is superseded by
decision 0012. The disjoint-write requirement below stands unchanged and
becomes more load-bearing, not less.
**Superseded by:** 0012 (in part: the verifier-selection clause)
**Related:** decision 0004

This is one of the most consequential architectural decisions in the
project.

## Context

The evidence on multi-agent coding is unusually clear, and the split it
draws is not single-agent versus multi-agent. It is shared-state versus
disjoint-state.

Every pro-multi-agent result in the literature comes from work that is
either read-mostly (review, search, hypothesis testing) or explicitly
partitioned into non-overlapping files. Every condemning result comes from
agents writing into the same state.

### What shared-state concurrency measures

| Study | Finding |
|---|---|
| 33,596 real pull requests, 2,807 repositories | Intra-agent textual conflict 19.8 percent; cross-agent 41.7 percent, non-overlapping confidence intervals. 42 percent of conflicts structural: agents disagree about whether a file should exist. |
| Constructed real-helper tasks, 108 runs | Blind interference in 105 of 108 runs, 97 percent. |
| 360 executions on frozen feature pairs | Uncoordinated parallel pair pass 23.3 percent. Admission control lifts it to 50.0 percent and integration to 96.7 percent, but serializes 96.7 percent of executions. |
| A 16-agent compiler build | Every agent hit the same bug, fixed it, and overwrote each other's changes; having 16 agents running did not help until differential testing gave them disjoint work. |

A design of many workers on different providers sits in the 41.7 percent
bucket, roughly double the same-agent conflict rate.

The known fix for shared-state conflict is serialization, which destroys
the wall-clock argument for parallelism entirely.

### What disjoint parallelism measures

Repeated independent sampling on a coding benchmark goes from 15.9 percent
at one sample to 56 percent at 250 samples, beating the then-best
single-sample result, with coverage scaling log-linearly over four orders of
magnitude.

The load-bearing caveat: this works only where an automatic verifier
exists. Without one, voting and reward models plateau. Software has
verifiers by default, which is precisely why this regime is available here.

### The encouraging number

In the interference study above, a coordination message of roughly 130
tokens recovered 82 to 98 percent of the lost ground. The interference is
coordination, not capability. That is the most encouraging figure in the
entire body of evidence, and it justifies investing in the messaging design
rather than in more agents.

### Why asynchronous delegation

A delegation design that acquires a per-target lock and then runs the
delegate's reply inline on the calling thread blocks the caller for as long
as the delegate runs; a three-deep chain can occupy three execution threads
for tens of minutes with the caller unable to do anything else.

The graph interface depends on concurrent workers being genuinely
concurrent, so this is not merely an efficiency concern.

### Why a report delivery policy

A four-kind delivery policy for worker reports, described below, is the
most token-frugal inter-agent messaging design found in any surveyed
reference. A live graph plus chatty workers will otherwise bankrupt the
user.

## Decision

**Workers read widely and write disjointly.** Concurrent workers never edit
the same working tree. Each writing worker gets an isolated git worktree.

Where the same unit of work is attempted more than once, a verifier selects
the winner: the test gate, the compiler, or the type checker, not a model's
opinion.

**Delegation is asynchronous.** The orchestrator issues a brief, records a
parent run id, receives a handle immediately, and keeps working.

**Worker reports follow a four-kind delivery policy**, not immediate
delivery of everything.

| Report kind | Delivery |
|---|---|
| informational | batched, up to an hour |
| needs-input | immediately, without interrupting |
| stuck | immediately, plus a rate-limited interrupt |
| first done | opens a settlement window, does not terminate |

## Consequences

**Every new worktree is a from-zero build environment.** Build outputs and
dependency directories are untracked, so git cannot carry them over. A
worker pays a full cold build unless kGraf pre-seeds or links those
directories itself. This is a real and possibly decisive cost, and it must
be measured before the worktree model is assumed viable. If cold-build cost
exceeds the value of the parallel attempt, fall back to one writer.

**Worktree lifecycle shells out to the git command-line interface, not to
an embedded git library.** A library's validation can return success when
the directory does not exist, its lookup can return a generic error rather
than not-found, and per-worktree configuration has open unresolved issues in
some libraries. Users already have git installed. Keep an embedded library
for in-process reads, diffing, and status only.

**Worktrees are created locked with a kGraf-tagged reason and pruned
unconditionally on startup**, so a crash leaves a recoverable locked entry
rather than a silent orphan on git's months-long expiry timer.

**Submodule repositories need an explicit post-add step**, and it is
neither fast nor automatic. Git's own documentation flags submodule support
in multiple checkouts as incomplete.

**Port six delegation guards wholesale**: roster membership rather than
permission alone, a delegation trace that must end with the sender, cycle
rejection by trace membership, a depth cap, a message size cap, and a policy
table of allowed pairs. Each exists because the corresponding failure
happened somewhere.

**Use deterministic delegation session ids** derived from sender, target,
and a hash of the root session, so the same pair in the same conversation
reuses one session. The id is directly usable as a stable edge id on the
graph.

**Two delegation modes**: peer-to-peer between standing workers that keep
their own context, and ephemeral context-isolated fan-out for cheap
exploration.

**Within a turn, schedule tool calls by a layered topological sort over
each tool's read and write sets.** Independent calls overlap, read-after-write
forces a later batch, and cycles break deterministically. kGraf's tools
write to a shared code graph, so this matters more here than it does in a
single-writer system.

**Mark parallel-safety opt-in per tool, defaulting to false.**

**Gate orchestration machinery on there being a team.** A solo install pays
zero tokens for multi-agent scaffolding.

**Sandboxing is weaker on Windows and the interface must say so.** No
non-admin mechanism there blocks network egress, so a worker can exfiltrate
the repository regardless of filesystem containment. Do not show one shared
"sandboxed" badge across platforms; show a per-platform strength indicator.
Details and the per-platform mechanism choice belong to decision 0011.

## Alternatives considered

**N agents co-editing one working tree.** Rejected on the 23.3 percent
pair-pass rate, the 41.7 percent cross-agent conflict rate, and the
documented 16-agent compiler-build failure above.

**Co-editing with admission control.** Rejected: it works, reaching 96.7
percent integration success, but it serializes 96.7 percent of executions,
so it is serialization wearing a parallelism costume. If the work must
serialize, serialize it honestly and spend the tokens on one agent.

**One writer, many readers, no parallel attempts.** A safe subset, and it
is the fallback if worktrees prove too expensive. Rejected as the default
because it forfeits the one regime with strong published evidence behind
it.

**Isolated worktrees with verifier selection.** Chosen.

## Amendment (2026-10-09): a worker stopped before it finished still has its tree verified

A worker's claim of completion is not progress, and the lack of one is not failure: the
verifier's verdict on the tree is. A worker that the stall check or the round budget ended
used to be an error attempt before any verifier saw its tree. A worker whose tree passed the
contract's own check, and that then looped on a cosmetic edit until the stall check ended
it, had its delivery discarded and the run reported as failing verification.

- **Rule.** An attempt that ended stalled or out of rounds and changed files goes to the same
  verification a completed attempt does, with the same scope check first. A pass accepts the
  tree as verified and adds a notice that the worker was stopped before it finished. A
  failure fails the attempt as the stop alone did; the live tally follows the verdict.
- **A check that checks nothing cannot accept it.** With a vacuous acceptance (`true`) and
  no gate command, the pass becomes a failure: the worker made no claim, so nothing would
  stand behind the tree.
- **A verifier that cannot run** leaves the attempt failed on its stop, counted as before,
  and not retried as a verifier outage.
- **Not applied** to a failed worker (it can end inside a round whose calls ran only in part,
  and is mostly a provider or environment fault with its own retry), a handed-back worker
  (continued by a fresh one), or a cancelled one. Both covered endings stop at a round
  boundary.
- **The stall check is unchanged.** A passing build or test since the last write does not
  reset its count: it changes nothing an identical call would see, and counting it would let
  a worker cycle read, failed edit, passing test until the step budget ran out.
- **Cost.** One verify run per such attempt, the same run a completed attempt gets.
