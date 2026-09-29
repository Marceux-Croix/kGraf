# Decision 0044: An adapter is verified against a running speaker, not against its own mock

**Status:** Accepted
**Related:** decisions 0041, 0042

## Decision

**An adapter to an external protocol is not accepted until it has exchanged a request and a
response with a real speaker of that protocol.** A mock restates the assumption the adapter
was written from, so a green test suite built only on mocks is evidence about internal
consistency and no evidence about the wire.

Mocks stay. They are the right tool for error paths, accounting behavior, and refusal logic,
and they run in an offline test suite. What changes is that they are no longer the only thing
an adapter is measured against.

Every protocol adapter carries:

1. **One test that asserts the request document field for field** against the published
   schema, separate from any round-trip test. It fails when the shape is wrong even if every
   mock in the file agrees with it.
2. **A recorded exchange with a running implementation**, naming the implementation and its
   revision.
3. **The published schema's own field names**, not plausible ones.

## Context

An early adapter for a typed-decision protocol landed with nine passing tests and was wrong
in five ways at once. The published wire format takes a state value, a question map keyed by
name, a required model field, and answers with a map keyed by the same names whose yes/no
probability sits in a field with a distinct, non-obvious name. The client instead sent a state
array of labelled blocks, a question array, no model field, and read an answer array looking
for a differently named probability field.

The missing required field alone settles it: the server rejects the document at validation,
so no request that client could compose would ever have reached a model. The adapter was not
approximately right; it was inoperative, and nothing in the repository could tell, because
every test mocked the shape the client already believed. The mock responded with an answer
array because the author wrote an answer array. The round-trip closed, the assertions held,
and all of it described a server that does not exist.

A test that asserts a client parses its own invention proves nothing about a protocol, in the
same way a test asserting a configuration value persists proves nothing about the effect it
claims.

Standing a real server up produced the first validation error within minutes. No amount of
reading the client would have found the defect, because the client was internally coherent.
The cheapest verification available was the one that involved the other side.

## Alternatives considered

**Leave the client as it was and note the risk.** The client cannot make a single successful
call, so this is a broken adapter, not a risk to flag.

**Correct the format and keep mock-only tests.** Fixes the instance and leaves the method
that produced it; the next adapter has the same exposure.

**Require a live exchange in the automated test gate.** Puts a network dependency and a large
model checkpoint in a gate meant to run offline and fast. The live exchange belongs in a
recorded evidence file instead, dated once.

**Verify against a running speaker, record it, and add a field-for-field schema test to the
gate.** Chosen. The gate stays offline and fast; the claim that the shape is right is checked
against something that did not come from the adapter's own author.

## Consequences

The corrected client speaks the published format: state value, required model field with a
default alias, question map, answer map keyed by question id, and the correctly named
probability field. A bearer key is sent when the user has configured one, because these
servers support authentication even though they can start without it.

A probability-only answer carries no confidence figure, and none is invented. The format
publishes a bare probability for a yes/no question and nothing else, so the answer type holds
an optional confidence value rather than a synthesized one.

Names key both directions, so a duplicate question id is refused before anything is sent
rather than silently answering the wrong question under the wrong name.

A scored answer is an expectation over level indices generated from the caller's declared
range, not a raw score in the server's own range, so the same answer type means the same
thing on every backend. Out-of-range values are clamped rather than wrapped.

Closing any dependent work now requires a recorded exchange with each protocol it claims to
speak, not just passing mock-based tests. The added schema test needs no server and runs in
microseconds, so the gate is unchanged in cost.
