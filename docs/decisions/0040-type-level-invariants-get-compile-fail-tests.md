# Decision 0040: An invariant enforced by the type system gets a compile-fail test

**Status:** Accepted, and it corrects a claim in decision 0039.
**Related:** decisions 0006, 0039

This record exists because decision 0039 recorded a gap as unavoidable, and
it turned out to be avoidable. The claim is corrected here rather than
edited there, matching the immutability rule for decision records.

---

## Context

Decision 0039's last consequence stated, in part: there is no negative test
for the accounting guarantee, and there cannot be, because the failing
program cannot be expressed in the language. That was the strongest
available form of the guarantee and the weakest available form of its test.

Naming that tradeoff was right. Accepting it was wrong, and the second half
of the claim is false.

The reasoning that produced it was: a test asserts that running a program
produces an outcome, so an invariant that prevents the program from existing
has nothing to run and therefore nothing to assert. That holds only if
"test" means "run a program." Compilation is an observable outcome too, and
asserting on it is a standard, mature practice: it is how a language's own
test suite and most macro-heavy libraries check that misuse is rejected.

The exposure this leaves is precise and worth stating, because it is the
whole reason the gap mattered. The accounting guarantee lives entirely in
one function signature and the privacy of four struct fields. Any of the
following would dissolve it while leaving every existing test green:

- adding a public constructor, or a type conversion, for local convenience
- adding a default-value derivation, which is close to a reflex on a small
  struct
- making a field public during a refactor that needed to read it
- relaxing the trait method to accept an optional token

None of those looks like a change to the accounting system. Each is exactly
the kind of locally sensible change decision 0039 predicted, and the whole
point of moving the rule into the type system was that a worker would not be
able to break it by accident. Leaving the signature itself unguarded put the
guarantee one reflexive shortcut away from gone.

Verified rather than assumed: adding a public constructor to the token type
turns the compile-fail test red, and removing it turns it green again. That
check is what distinguishes this from a test that passes for the wrong
reason.

## Decision

Where an invariant is enforced by the type system rather than by a runtime
check, it gets a compile-fail test: a fixture program that attempts the
forbidden thing, and a test that passes only when the compiler rejects it.

For the accounting guarantee this covers three fixtures: fabricating a
ledger token by struct literal, calling a constructor on it, and obtaining
one through a default-value derivation.

A compile-fail fixture must be verified to fail for the right reason. The
expected compiler diagnostic is committed alongside each fixture, so a
fixture that starts failing for an unrelated reason is a test failure rather
than a false pass.

## Consequences

These fixtures compare exact compiler output and will break on a compiler
release that rewords a diagnostic. That is accepted deliberately, because
the failure direction is the right one: a spurious red costs a minute, and a
silently lost accounting guarantee costs a wrong invoice. A loud false alarm
beats a quiet false pass.

This generalizes, and is why it is a decision rather than a single change.
Other invariants that must survive being edited by the workers they
constrain, write-scope isolation and concurrency caps among them, are
candidates for the same treatment. Each one enforced by a signature rather
than a runtime check gets fixtures under the same convention.

The general rule, stated so it outlives this record: "the failing program
cannot be expressed" is a description of the guarantee's strength, never a
reason to skip its test. If the compiler is the thing enforcing the
property, the compiler's verdict is the thing to assert on.

## Alternatives considered

**Leave the gap, as decision 0039 recorded it.** Rejected on the exposure
above. A one-line convenience addition should not be able to silently
disable call accounting.

**Assert the structure instead: a test reading the source for a public
constructor.** Rejected: it tests the source text rather than the
mechanism, and it is defeated by any spelling the search did not anticipate.

**A runtime check as well: panic if a provider call has no open ledger
row.** Rejected for the reason decision 0039 already gave: it catches the
violation in production, after the spend.

**A compile-fail test, with the expected diagnostic committed.** Chosen.

## Reactivation trigger

A type-level invariant that this style of test cannot express, or a
compiler change making the diagnostics unstable enough that the fixtures are
maintained more often than they are useful. Neither is expected; both would
reopen the question of what else could watch a signature.
