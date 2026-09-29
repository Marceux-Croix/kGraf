# Decision 0078: The handshake comparator reads qualified names and signatures; interface checks stay off by default

**Status:** Accepted
**Related:** decisions 0026, 0030, 0031, 0033, 0035

## Context

kGraf's optional interface checks include a "handshake": each side of an interface states, in
plain prose, what it expects from the other, and the orchestrator compares the two statements
mechanically. In its first live test against real model providers, the comparator flagged
seven mismatches. On inspection, six were noise: real models write a symbol's fully qualified
name rather than its bare name, write out a full type signature rather than restating it word
for word, and sometimes describe their own provisions more fully than the comparator expected.
Only one of the seven flagged a genuine disagreement between the two parties.

## Decision

**The comparator is widened to read a symbol however a real model actually writes it**: a
fully qualified name, a bare name, or a full signature are all recognized as referring to the
same underlying symbol, rather than requiring an exact, word-for-word match. A statement about
a party's own symbol is no longer treated as if it were a claim about the other party.

Replayed against the earlier live test, the comparator now reports only the one genuine
disagreement, and continues to catch every deliberately constructed real mismatch used to test
it.

**Interface checks stay off by default.** The widened comparator is a necessary condition for
turning them on by default, but not yet sufficient: an unrelated default limit elsewhere in
the pipeline still cuts off a large enough decomposition before it reaches these checks at all,
and no live run with the checks turned on has yet reached the point of actually dispatching
work.

## Consequences

- The comparator's known blind spots are documented and grow slightly narrower rather than
  wider: a bare type is still matched loosely to a full signature containing it, and a symbol's
  containing module or file path is not itself compared, since an earlier build-time check
  already proves the two files reference the same real symbol.
- The condition for eventually turning interface checks on by default is now explicit: a live
  run, with an adequately sized decomposition limit, that reaches dispatch with the handshake
  passing.

## Alternatives considered

- **Loosen the comparator with a fuzzy text-similarity score.** Rejected: the handshake is
  meant to be a precise, symbol-level comparison; a similarity score has no clean line between
  harmless rewording and an actual, meaningful change.
- **Ask models to write bare symbol names instead of qualified ones.** Rejected as the fix:
  the handshake is explicitly testing what models actually say unprompted; a comparator that
  only understands one specific style of answer will keep failing on the next model provider
  tried.
- **Flip the default on immediately, since the comparator now behaves correctly.** Rejected:
  a correct comparator is necessary but not sufficient; a separate limit elsewhere in the
  pipeline, and the lack of any full passing live run, are both still open.
