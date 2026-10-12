# Decision 0023: Solving collisions does not make concurrent agents work, and a correction to 0022

**Status:** Accepted. Corrects a factual error in decision 0022. Challenges the
premise, shared by decisions 0012, 0016, and 0022, that collision control is
what stands between kGraf and working concurrency.
**Amended by:** 0024 (the go/no-go criterion)
**Related:** decisions 0009, 0012, 0016, 0018, 0022

---

## The correction

Decision 0022 and its supporting measurement state that 33 percent of
textually clean merges had build or test conflicts. That is wrong, and the
source paper states it wrong too.

Recomputed from that paper's own data, across 1,694 merges in the three
systems with real test suites:

| Outcome | Count | Share of all merges |
|---|---|---|
| Textual conflict | 266 | 15.7% |
| Clean text, build failure | 24 | 1.4% |
| Clean text, builds, test failure | 109 | 6.4% |
| Fully clean | 1,295 | 76.4% |

The real figures: higher-order conflicts (conflicts that only appear after a
clean merge, at build or test time) are 33.3 percent of all conflicts (133 of
399), and 9.3 percent of textually clean merges (133 of 1,428).

The paper's abstract is correct. Its conclusion sentence and one of its
sections are garbled, dividing the higher-order count by the conflict count
rather than by the clean-merge count, and the error propagated into the
citing literature and into decision 0022.

Use 9.3 percent, or "one in three conflicts is higher-order." Not "one in
three clean merges is broken." The semantic problem is real and smaller than
first recorded.

## The finding that matters more

A benchmark built for exactly this question was published: over 600
collaborative coding tasks across 12 libraries and 4 languages, where two
agents are given different features that can be implemented independently but
may conflict. Ground-truth solutions were written jointly to verify
compatibility.

| Result | Value |
|---|---|
| Two-agent cooperative success, frontier models | 25% |
| Same models, one agent doing both features | ~50% |
| Coordination gap | ~50% relative |
| Success at 2 / 3 / 4 agents | 68.6% / 46.5% / 30.0% |
| Failure causes | Expectation 42%, Commitment 32%, Communication 26% |
| Effect of a communication channel on task success | not statistically significant |
| Effect of a communication channel on merge conflicts | significantly reduces them |

That last pair is the result. Giving agents a channel measurably reduced
merge conflicts and did not improve whether the work succeeded.

Collision avoidance is not the binding constraint. Every mechanism in decision
0022 attacks a problem that, on this evidence, was not the one stopping
concurrent agents from working.

And adding agents makes it worse monotonically: success falls from 68.6 to
46.5 to 30.0 percent as the team grows from two to four.

## What this does and does not overturn

It does not overturn decision 0022's mechanisms. Runtime leasing,
similarity-based scheduling, and speculative execution all still do what they
were measured to do. Locking still prevents textual collisions exactly.

It overturns what those mechanisms were expected to buy. They were adopted on
the assumption that collisions were the obstacle. They are not, so solving
them completely still leaves the coordination gap.

## Decision

Concurrency stays in the design, and its justification changes. It is no
longer justified by "collisions are solvable, therefore parallel agents
work." It is justified only if kGraf can measure a coordination benefit on
its own work.

Before concurrency is built past a prototype, kGraf runs two agents against a
solo baseline on the same tasks and compares task success, not collision
counts. If the solo baseline wins, kGraf ships sequential specialization:
route tasks to whichever vendor's model fits the task, and drop concurrency.

The failure taxonomy becomes the design target. Expectation (42%) and
commitment (32%) together account for three quarters of failures. Those are
about agents misjudging what a peer will deliver and failing to hold to what
they said, which is a brief-and-contract problem, not a locking problem. That
is where design effort should go.

kGraf does not scale agent count as a lever by default. The measured curve
falls monotonically past two agents, so any concurrency cap defaults at the
low end of its range, and any ramp toward higher concurrency requires
evidence of benefit rather than merely evidence of safety.

## Alternatives considered

None recorded separately; this record corrects and narrows decision 0022
rather than choosing among design alternatives.

## Caveats, because this is one preprint

This benchmark is a preprint, a single benchmark, from a single author team.
Its tasks were constructed to be conflict-prone, with 77.3 percent of pools
having conflicting ground-truth solutions, so it is a hard case by design and
not a representative sample of real work.

It is also the only benchmark that exists for this question, and its
direction agrees with other measurements: an admission-control study that
recovered reliability by serializing 96.7 percent of executions, a 27.67
percent conflict rate across 107,026 agent pull-request merge simulations,
and 41.7 percent cross-agent against 19.8 percent intra-agent conflict on
replayed co-active pairs.

No result found anywhere in this research says concurrent multi-agent coding
works. That is worth stating plainly before more is built on the assumption
that it does.
