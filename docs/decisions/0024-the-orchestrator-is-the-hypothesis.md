# Decision 0024: The orchestrator is the hypothesis, and it is tested with three arms

**Status:** Accepted. Refines decision 0023's go/no-go criterion, which had two
arms. Strengthens decision 0004's justification for a single director.
**Amends:** 0023 (the go/no-go criterion)
**Related:** decisions 0004, 0012, 0022, 0023

---

## Context

Decision 0023 recorded that giving agents a communication channel
significantly reduced merge conflicts and had no significant effect on task
success, with failures attributed to expectation (42%), commitment (32%), and
communication (26%). It concluded that collision avoidance is not the binding
constraint.

That conclusion stands. What it left unexamined is what topology was being
measured.

### The benchmark tested peers, not supervision

The measured configuration is two agents given different features, with and
without a channel between them. There is no orchestrator arm: an agent that
writes the brief for both workers, owns the interface between them, and
verifies delivery against a specification. Nobody in the benchmarked setup
owns the interface between the two features, nobody verifies delivery against
a specification, and the shared context is whatever the agents choose to tell
each other.

The three failure causes map directly onto what a single director structurally
removes:

| Failure | Share | What a single director changes |
|---|---|---|
| Expectation | 42% | It writes both briefs, so it owns the interface. Neither worker infers what the other will deliver. |
| Commitment | 32% | Delivery is checked against an issued brief rather than peer-trusted. |
| Communication | 26% | The director is the shared context; nothing depends on workers electing to share. |

So the benchmark does not refute orchestrator-supervised specialization. It
also does not support it. The configuration is untested.

The correct response to reasoning of this shape is not to assume the
orchestrator fixes the coordination gap. It is to test the configuration that
was missing.

### Three reasons it might not work

The orchestrator is itself a language model. If it misjudges what a
decomposition requires, both workers receive briefs built on the same wrong
assumption, and the expectation failure has moved up a level rather than
disappeared.

Coordination alone was not sufficient in the largest published multi-agent
build attempt found. A 16-agent build using task-claim files still recorded
every agent hitting the same bug and overwriting each other's work, until
differential testing forced the work disjoint. That is weaker than an
intelligent supervisor writing interface contracts, but it is the closest
published data point and it is negative.

It costs. Writing tight interface contracts for every concurrent pair is more
orchestrator effort and more latency, against an entity that is already a
context bottleneck. Context management for the orchestrator itself is an
unsolved problem, and this adds load to it.

## Decision

The go/no-go test for concurrency runs three arms, not two: solo,
peer-coordinated, and orchestrator-supervised. Comparing only two agents
against one agent cannot answer the question the design actually rests on.

The orchestrator owns the interface between concurrent briefs, not merely
each brief in isolation. Writing two briefs that compose is a distinct
obligation from writing two briefs that are each individually clear.

Commitment is verified, not trusted. A worker's delivery is checked against
the brief the orchestrator issued.

## Consequences

Brief quality becomes the highest-leverage property in the system, ahead of
model routing, ahead of context management, ahead of collision control. If
expectation and commitment are three quarters of failures and briefs are what
address them, that is where design effort belongs.

Interface ownership is a named orchestrator obligation. When two briefs are
dispatched concurrently, the orchestrator states what each will deliver to
the other, in the briefs themselves. Two individually clear briefs that
assume different interfaces is precisely the expectation-failure case.

Delivery verification is structural, not advisory. A worker returns a
compact structured result, and the orchestrator checks it against the brief
before folding it in. A result that does not satisfy its brief is a failure
to surface, not a variation to absorb.

The case for a single director broadens. It was argued on the grounds that a
committee gives the user nobody to talk to, which is a product argument.
There is now a performance argument alongside it: a supervisor owns
contracts that peers structurally cannot. Both survive; the second is a
hypothesis and is labeled as one.

The three-arm result decides the design's shape. If orchestrator-supervised
work beats solo, concurrency is justified and the orchestrator is why. If it
beats peer-coordinated but loses to solo, the orchestrator helps and
concurrency still does not pay, and sequential specialization is the answer.
If it matches peer-coordinated, the supervision hypothesis is falsified and
the case for a single director rests on its product argument alone.

## Open questions

How briefs should express an interface contract is unspecified. Natural
language between two workers is what the failing peer configuration already
had. Something more structured is likely required.

Whether the orchestrator can detect its own bad decomposition before
dispatch, rather than after both workers fail, is unaddressed, and has no
proposed mechanism yet.

## Alternatives considered

**Accept the two-arm test from decision 0023.** Rejected. Solo against
peer-coordinated measures what the benchmark already measured. It would tell
nothing about the topology kGraf actually builds, and a negative result would
be attributed to the wrong cause.

**Assume the orchestrator resolves the coordination gap and proceed.**
Rejected as taking a plausible mechanism as established because it is
architecturally convenient.

**Three arms.** Chosen. The marginal cost is one additional arm on an
experiment already required.
