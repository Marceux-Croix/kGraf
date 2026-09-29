# Decision 0076: Each consumer of an interface writes and gates its own test of what it expects from the producer

**Status:** Accepted
**Related:** decisions 0023, 0026, 0027, 0029, 0030, 0031, 0033, 0037

## Context

Measured across concurrent multi-agent work, one of the largest single causes of failure is
one agent misjudging what a piece of work it depends on will actually deliver. An earlier
mechanism already has each side state, in plain prose anchored to the interface's own symbols,
what it expects the other to do; the orchestrator compares the two statements mechanically.
That comparison is still a comparison between two written claims, not a check of actual
behavior: it cannot catch a paraphrase, an unstated assumption, or any behavior neither side
happened to put into words.

## Decision

**Before a producer of an interface is dispatched, each of its consumers writes one automated
test of what it expects from that interface**, anchored only to the interface's declared
symbols, and never given the producer's placeholder implementation or its real one in advance.

**A consumer's test is gated exactly as the orchestrator's own interface test already is**: it
must compile against a trivial placeholder, then it must fail against that placeholder and
pass against a correct reference implementation. A test that does not clear both checks is
rejected and never added.

**An accepted test becomes part of what the producer must satisfy.** A producer that fails an
accepted consumer test is challenged by name, naming the specific expectation it failed,
rather than receiving a bare, unattributed "tests failed."

This is the same idea known outside kGraf as consumer-driven contract testing: the party that
depends on an interface writes the test of it, not the party that builds it. What that
practice normally asks a person to do by hand, kGraf automates and checks twice, using the
same route (and so the same model) that will go on to build the consumer's own side of the
work, so the test reflects that model's actual reading of the interface.

## Consequences

- This is complementary to, not a replacement for, the earlier prose-comparison mechanism: a
  written test catches exactly the class of failure (unstated behavior) the prose comparison's
  own record already says it cannot catch.
- The mechanism is additive and non-blocking: a consumer that cannot produce a useful test, or
  whose route is unavailable, does not stop the run.
- At most one accepted test per consumer-producer pair per version of an interface is kept, so
  re-checking the same interface never repeats the same work.

## Alternatives considered

- **Have the orchestrator write the consumer's test itself.** Rejected: this reproduces
  exactly the failure the mechanism exists to catch, since a wrong assumption baked in by the
  same party that also wrote the producer's own contract is never independently checked by
  anyone.
- **Block the producer's dispatch entirely on a rejected or missing consumer test.** Rejected:
  this mechanism adds a way to detect a problem; it should not add a new way for a run to
  stall outright.
- **Run every accepted test through a generic quality gate that reports only "tests failed."**
  Rejected: an unattributed failure gives the orchestrator nothing to act on; naming the
  specific consumer and expectation is what lets it decide what to do next.
