# Decision 0033: Dependency is observed and subscribed to, not predicted

**Status:** Accepted. Closes the open question in decision 0031 on whether an
agent can state what it assumes will not change.
**Related:** decisions 0007, 0018, 0019, 0020, 0022, 0026, 0031

---

## Context

Decision 0031 specified the handshake's third element as what an agent
assumes will remain stable, and flagged that this is close to the
blast-radius prediction problem already found not to work.

The two are different problems, and conflating them was the error.

The earlier, falsified problem is forward blast radius: given a change to X,
what else must change. That is a forecast, over an entire repository, with
no bound. A call-graph-based predictor of this was measured and its verdict
was near-independent of the truth.

Element 3 is backward: what did this agent rely on. That is not a forecast.
It is observable. The agent reads specific files and calls specific
functions, and the harness watches every read and write already.

### Why subscription beats declaration

The timing objection is real: at handshake time the agent has not read
anything, so observation cannot help yet.

Subscription sidesteps it entirely. Rather than asking what the agent
assumes is stable, the harness watches what the agent reads and notifies it
when any of that changes. The agent predicts nothing. This converts a
prediction problem into an invalidation problem, which is a solved pattern.

The machinery exists. A change log is already the single event spine, so
every mutation is already observed. "Notify every agent whose read set
includes this node" is a query against it, not a new subsystem.

### Why the stated question must be bounded

"What will not change" is unbounded over the repository and unanswerable for
the same reason forward prediction is.

"What I assume about the interfaces I consume" is a small enumerated set,
because the contract already names those interfaces. An agent can answer
that, and answering it catches the expectation mismatch the handshake exists
to catch.

### Why two phases

The initial handshake is made before reading, so it is a guess. Its value is
cheap and real: it surfaces gross misunderstanding before effort is spent.

After reconnaissance, when the agent has read what it needs but written
nothing, the dependency set is derived rather than stated. That version is
exact, and it still precedes any writing, so it precedes any collision.

## Decision

An agent does not declare what it assumes will remain stable. It is
subscribed to what it actually reads, and notified when any of it changes.

The handshake question is bounded to the contract surface. What an agent
states is what it assumes about the interfaces it consumes, which the
contract already enumerates.

The handshake has two phases. An initial statement before reading, which is
a cheap guess with low stakes, and a revised dependency set derived from the
observed read set after reconnaissance and before any writing.

## Consequences

Read sets become first-class state, alongside write scopes. The harness
already observes both; this makes the read set durable and queryable rather
than transient.

Invalidation is an event on the existing spine. A node changing notifies
subscribed agents, which is the same fan-out shape as the contract-
inadequate report described in decision 0027.

An agent may be notified of a change it does not care about. Reading a file
is not the same as depending on its contents. This is an over-approximation,
and it is the correct direction to err: a spurious notification costs a
message, while a missed one costs silent staleness.

The handshake becomes cheaper. Bounding the stated question to the contract
surface removes the open-ended part, which is the part that would have
consumed effort producing an unreliable answer.

## The residual, stated plainly

None of this catches an assumption the agent never read.

"I assumed the parser is idempotent" is behavioral, appears in no read set,
and is invisible to subscription. Assumptions about behavior rather than
about structure are exactly the semantic class that version control cannot
see, that a static predictor could not measure, and that published studies
found most of their damage in.

Gates catch that class or nothing does. This is another load gates carry
that earlier records did not anticipate, and it is worth recording that
their justification has now grown three times: quality, then cold-start
safety, now behavioral assumption failure.

## Alternatives considered

**Ask the agent to predict its dependencies.** Rejected. It is the falsified
problem in reverse, and there is no reason to believe an agent does it
better than a static predictor did.

**Drop element 3.** Rejected. Undeclared dependency is the failure mode that
defeated a published admission-control mechanism, where 45 of 46 blocks
targeted files already declared but at too coarse a granularity.

**Derive it only from observation, with no stated component.** Rejected on
timing: observation is empty at handshake time, and the initial statement is
what catches gross misunderstanding before effort is spent.

**Subscribe without bounding the stated question.** Rejected. Subscription
handles what was read; the stated question handles what is expected of a
counterparty, which is not the same set and is not observable at all.
