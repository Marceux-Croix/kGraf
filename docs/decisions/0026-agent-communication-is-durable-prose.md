# Decision 0026: Contracts are signature plus test; agent communication is durable plain-English markdown in the graph

**Status:** Accepted. Closes the question left open by decisions 0024 and
0025 on how an interface contract is expressed. Refines decision 0004 and
0012 on agent identity lifetime.
**Related:** decisions 0004, 0005, 0007, 0012, 0013, 0018, 0019, 0022, 0024,
0025

---

## Context

### Why signature plus test, and why it is an artifact

Decision 0024 established that expectation failure, at 42 percent of
multi-agent failures, is what a supervisor writing both briefs addresses, and
left the expression of the contract open. Decision 0025 added that plain
prose alone was what the failing peer configuration already had.

Making the contract an artifact in the repository rather than a message
between agents resolves three constraints at once:

- **Cross-vendor.** A file is read identically by every provider's model. No
  schema compliance variance, no structured-output support differences, no
  serialization negotiation. This matters because kGraf is explicitly
  heterogeneous across model vendors.
- **Verification.** A structural mismatch becomes a compile error and a
  behavioral mismatch becomes a test failure, rather than a silent divergence
  discovered at integration.
- **Reviewability.** It appears in a diff.

It also composes with other design decisions rather than adding machinery:
tests are already part of the definition of done, the harness already
discovers the test runner, and gates already validate committed artifacts.

### Why the communication itself must be prose

Structured messages are cheaper in tokens and easier to machine-check. They
are rejected for the content of agent-to-agent communication because the
record has to be intelligible to a person without a decoder.

This is a deliberate cost: prose is more tokens than structure, and a bounded
delivery policy (batching low-priority messages rather than streaming them)
is what keeps the volume bounded.

The distinction that keeps it workable: the message envelope carries
structure, and the content carries English. A worker's result may carry a
structured payload, but the message conveying it says in plain language what
was done and what it means.

### Why stable pairwise files, and what that implies about identity

A permanent file per pair of communicating agents gives a relationship
continuity that per-session logs do not. An agent returning to a
collaboration after an interval finds what was previously established rather
than rediscovering it.

This requires durable agent identity. Treating workers purely as spawned and
retired processes would make a recurring pair impossible.

The reconciliation: identity is durable and tied to role and scope; the
process is ephemeral. The same identity may be instantiated many times.
Precedent exists in at least one other multi-agent framework's deterministic
delegation session identifiers, derived from source, target, and root so
that the same pair in the same conversation reuses one session.

Identity must be semantically meaningful for file reuse to make sense. An
identity that was a parser worker and is later a documentation worker should
not inherit the parser conversation. Identity therefore binds to role and
scope, and a materially different role is a different identity.

### Why the message bus is the sole writer

Committing conversation files creates precisely the collision class this
project spent its research budget on: two agents appending to a shared file
concurrently.

The fix follows a well-established pattern: single writer per file, with
agents writing only inside their own directory. Here the message bus owns
every conversation file and appends to it, matching a rule that state changes
flow through one spine.

### Why they are indexed

A reference code-indexing implementation already indexes markdown, with
headings as nodes and bodies captured as properties. Indexing conversations
makes them first-class retrievable context: an agent can search for what was
previously decided about a subsystem and find the conversation that decided
it, through the same tools it uses to search code.

This is also what makes an agent's transcript concrete on the interface:
clicking an agent shows its messages, and those messages are nodes with
edges to the code they concern.

## Decision

An interface contract is a signature and a test, committed before either
worker starts, wrapped in a prose brief. The orchestrator writes the type,
trait, or function declaration and the test both sides must satisfy. The
brief explains intent around them.

Agent-to-agent messages are plain English prose. Not structured blobs, not
schemas, not code fragments standing in for explanation. A human reading a
conversation understands what happened without decoding anything.

All agent communication is persisted as markdown in the repository, in a
dedicated folder, committed and versioned like any other artifact.

Communication files are keyed to stable pairs of agents and are permanent.
The same two agents always append to the same file, across any gap in time.

Agent identity is durable; the agent process is ephemeral. An agent is
identified by its role and scope, not by a running task, so the same
identity can be spawned, retired, and spawned again.

The message bus is the sole writer of these files. Agents never write them
directly.

The files are indexed into the working graph like any other markdown.

## Layout

Conversations live in `docs/agent-comms/`.

Naming is deterministic so a pair always resolves to one file, and readable
so a person can navigate without a lookup table. The scheme is the two
roles, ordered canonically, with identity suffixes only where roles collide:

```
docs/agent-comms/
  INDEX.md
  parser-impl--lexer-impl.md
  parser-impl--test-author.md
  orchestrator--parser-impl.md
```

Files are created lazily, only for pairs that actually communicate, so the
set stays sparse rather than growing with the square of the agent count.

## Consequences

Agent conversations become searchable project memory, not a debugging
artifact.

The repository grows with conversation. Volume is bounded by the delivery
policy, but this should be measured, and a retention or compaction policy may
be needed. Compaction must preserve the verbatim record, since the interface
promises unabridged messages.

Prose costs more tokens than structure. Accepted for auditability, and
recorded here so the tradeoff is not rediscovered as a defect.

Write scope and interface contract remain distinct. A write scope
declaration says what an agent will touch, for collision control. The
contract says what it will deliver, for expectation control. Neither
substitutes for the other.

A summary is derived and labeled as derived. Where a conversation is long
enough to need one, the summary is marked as generated with the verbatim
record one step away. A summary that can quietly diverge from what was said
is worse than none.

The orchestrator must be able to write a correct interface before
implementation exists. This is a real new obligation and the likeliest
failure point, since it is the same expectation failure flagged in decision
0024 and still has no detection mechanism.

## Open questions

What happens when a contract turns out to be wrong mid-flight is
unspecified. A committed signature that both workers have built against, and
which proves inadequate, needs a revision path that does not silently
invalidate work already done.

Whether conversation files should ever be pruned is unresolved, and
interacts with the promise of an unabridged record.

## Alternatives considered

**Templated prose alone, as the contract.** Rejected. It is what the failing
peer configuration already had, and nothing verifies it. Retained as the
wrapper around the signature and test.

**A typed declaration block in the brief, as the primary contract.**
Rejected: it needs a rendering layer for humans, and schema compliance varies
across providers, which is a cross-vendor liability.

**An interface definition language.** Rejected as too heavy for same-
repository work. Worth reconsidering at a genuine service boundary.

**Per-session conversation logs.** Rejected. They lose the continuity that
makes a returning agent cheaper than a new one.

**Storing conversations outside the repository.** Rejected. They would not be
versioned, reviewable, or indexed alongside the code they concern.

**Agents writing their own conversation files.** Rejected on the collision
class this project exists to avoid.
