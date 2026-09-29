# Decision 0031: The vacuity check runs against the stub and is two-sided; the handshake states assumptions about the counterparty

**Status:** Accepted. Closes both open questions in decision 0030.
**Related:** decisions 0022, 0023, 0026, 0029, 0030

---

## Context

### The vacuity hole, and why it closes for free

Decision 0030 required the contract's test to fail against an empty
implementation, and flagged that a test failing because nothing compiles
proves nothing about whether it asserts anything.

Decision 0030's second check already produces the fix. The stub-compile
check builds a trivial stub satisfying the producer contract. Running the
test against that stub rather than against emptiness guarantees compilation
by construction, so any failure is necessarily an assertion failure. The two
checks compose, and the hole was an artifact of running them independently.

### Why it must also be two-sided

There is a second vacuity mode decision 0030 did not name: a contract test
that nothing can satisfy. It fails against the stub, passes the check, and
then both workers fail against it. The failure presents as two bad workers
rather than one bad contract, which is the worst possible misattribution.

So the test must also pass against a correct reference implementation.
Failing against a wrong implementation and passing against a right one is
what establishes that the test constrains something achievable.

An optional, stronger version uses a second stub returning plausible-but-
wrong values rather than defaults or panics. A test that only catches an
unimplemented placeholder catches very little. This is mutation-style
discipline and is harder to generate, so it is a refinement rather than a
requirement.

### What the handshake should contain

Decision 0030 adopted the handshake as an experiment and flagged that
getting its content wrong is the likeliest reason it returns null.

The valuable content is not what an agent will do. That is already in its
brief, which the orchestrator wrote. Restating it adds tokens and no
information.

The valuable content is what an agent assumes about the other. Expectation
failure, at 42 percent of measured multi-agent failures, is precisely one
party assuming the other would deliver something it did not. That assumption
is never written down, which is why it diverges silently.

Four elements:

1. **What I will provide**, in the contract's terms.
2. **What I assume you will provide.**
3. **What I assume will not change** while I work.
4. **What I will touch**, which is the write-scope declaration used for
   collision control.

Element 3 is the one usually left implicit and the one that breaks. An agent
depending on something staying stable, without saying so, is the same class
of failure as the undeclared scope that defeated a published admission-
control mechanism measured elsewhere.

### Prose, and still mechanically comparable

Agent communication is required to be plain English. A structured diff
requires structure.

These reconcile through anchoring: the handshake is prose that names
contract symbols. A statement that `parse_config` returns a `Config` is
readable and still comparable at the symbol level. The orchestrator extracts
the symbol-level claims and compares them; a person reads the prose.

### Why the orchestrator compares, and why restating beats confirming

Agents must not be relied on to detect their own mismatch. They already
failed to, and that failure is the same 42 percent this mechanism addresses.
The orchestrator holds both statements and compares them.

And a counterparty must restate in its own words rather than confirm. An
agent asked to confirm an assumption will tend to agree, which produces an
echo rather than evidence. Two independent statements can be compared; a
statement and its acknowledgment cannot.

## Decision

The vacuity check runs against the stub, not against emptiness, and is
two-sided: the contract's test must fail against a wrong implementation and
pass against a correct one.

The handshake states assumptions about the counterparty, not a restatement
of one's own brief. Four elements, anchored to contract symbols.

The counterparty restates rather than confirms, and the orchestrator
compares the two statements. Agents do not check their own agreement.

## Consequences

The three pre-dispatch checks are now ordered and dependent. The stub is
built first, the vacuity check consumes it, and the handshake follows. They
are not independent and should not be run as such.

A reference implementation is needed for the two-sided check. For a small
contract this is cheap; for a large one it is not, which is another force
pushing toward smaller chunks.

Handshake mismatch is an orchestrator-detected event, joining existing
blocking states rendered on the graph: a node held pending handshake
reconciliation is distinct from dependency-blocked, collision-deferred,
concurrency-capped, and contract-under-review.

The experiment sharpens. Decision 0030 could not say what a null result
would mean, because the content was unspecified. With content specified, a
null result means agents cannot articulate their assumptions about each
other usefully, which is a finding about model capability rather than about
the design.

## Open questions

Whether element 3 can be answered at all. Stating what one assumes will not
change requires knowing what one depends on, which is close to the
blast-radius prediction problem already found not to work for a call graph.
An agent may be no better at this.

## Alternatives considered

**Keep the vacuity check against emptiness and accept the hole.** Rejected
once it became clear the stub check already closes it at no cost.

**One-sided vacuity check.** Rejected on the unsatisfiable-contract case,
which misattributes a contract defect to two workers.

**Handshake as a restatement of one's own brief.** Rejected. It is the
obvious design, it costs tokens, and it carries no information the
orchestrator does not already have.

**Structured handshake format instead of prose.** Rejected. Anchoring to
symbols gets mechanical comparison without giving up readability.

**Agents confirm each other's handshakes.** Rejected on sycophantic echo,
and because it delegates detection to the parties that already failed to
detect.
