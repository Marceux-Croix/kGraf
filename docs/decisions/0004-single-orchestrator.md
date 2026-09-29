# Decision 0004: A single orchestrator holds full directional control

**Status:** Accepted
**Related:** decisions 0001, 0003

## Context

The obvious alternative in multi-agent work is a set of peer agents that
negotiate among themselves toward a goal. That topology is slow, expensive
in tokens, prone to agents talking past each other, and it produces work
nobody chose.

The decisive argument is not efficiency though, it is that a committee gives
the user nobody to talk to. kGraf's premise is a single always-available
point of contact that remembers every decision and why it was made, and that
is accountable for the result. That requires one entity in charge.

Permitting direct worker-to-worker messages is a narrow concession to cost
and latency. When one worker needs something another already knows, a round
trip through the orchestrator pays for the same information three times and
adds a turn of latency. Direct messaging is allowed because it is cheaper,
not because workers have autonomy.

## Decision

One orchestrator agent holds full directional control. It is the only agent
the user addresses. It decomposes the plan into briefs, chooses the
provider and model for each brief unless the user pinned one, spawns
workers, and retires them.

Workers may send messages to one another directly, without relaying through
the orchestrator, but they work to briefs the orchestrator issued and it can
terminate any of them at any time.

The orchestrator refuses to begin building before an agreed plan exists. If
there is none, its first job is to produce one with the user.

## Consequences

The orchestrator is a single point of failure and a context bottleneck. Its
context will grow over a long session and will need compaction, which is a
known hard problem.

Worker-to-worker messages must still be recorded and rendered on the graph,
so the message bus records everything even though not everything passes
through the orchestrator. The user clicking an agent must see every message
it sent or received, direct or not.

Model and agent-type pinning by the user is an override on the
orchestrator's routing choice, not a separate mechanism. The orchestrator
chooses by default and the user may constrain it.

The orchestrator holds a mandatory precondition: no building without an
agreed plan. This is a behavior gate, not a suggestion in a prompt.

## Alternatives considered

**Peer negotiation among equals.** Rejected for the reasons above.

**Strict hierarchy with all messages relayed through the orchestrator.**
Cleanest to reason about and to visualize, and every message is on the
record in one place. Rejected: it triples the token cost of routine worker
questions and adds latency, for an auditability benefit that is obtained
anyway because kGraf records all messages regardless of whether they passed
through the orchestrator.

**Single director, direct worker messaging, orchestrator retains control.**
Chosen.
