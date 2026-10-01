# Decision 0086: An empty attempt is refused when its declared write target is still a placeholder, and every accepted empty attempt is reported

**Status:** Accepted
**Related:** decisions 0009, 0081, 0085

## Context

An attempt that changes no file still runs its check, and a passing check still counts as
verified. That is correct for a contract whose acceptance was already true: a re-verify, a
check-only contract, or a check whose own output does not commit anywhere. It is wrong for a
contract whose acceptance depends on replacing a placeholder: a check that only proves the
tree still builds, and never exercises the missing behavior, passes even when the placeholder
was never touched.

Decision 0081 already refuses a producer's delivery that leaves an interface's declared stub
byte-identical to what was committed for it, but only on the interface path, where the
original stub content is recorded and available to compare against. An ordinary contract has
no such recorded content, and an empty attempt's write target is byte-identical to what it
held at dispatch by construction, so byte-identity alone cannot tell a legitimately unchanged
file apart from an abandoned placeholder on this path. What tells them apart is what the
unchanged content is, not whether it changed.

## Decision

1. An empty attempt is refused, not passed, when a declared write target still holds nothing
   but a placeholder body: every function body in the file, once comments and surrounding
   whitespace are stripped, is exactly one call to `todo!` or `unimplemented!`, with nothing
   after it. `unreachable!` is not a marker: it states an invariant, and a mock of methods
   the code under test never calls is legitimately made of it. A signature is read to its
   body, so a `;` inside an array type does not end it. The marker call counts in every
   delimiter and path form (`todo![]`, `todo!{}`, `std::todo!()`), and comments and string or
   character literals are masked before the scan, so a brace inside one changes nothing. Only Rust source files are
   recognized today. A file with no function bodies, or with even
   one real one, is never flagged, so a legitimately unchanged file is never mistaken for an
   abandoned placeholder.
2. Every attempt accepted with no change now carries a notice saying so, alongside anything
   its check itself reported. This uses the run's existing notice path, so it reaches the same
   output a run's other notices already reach.
   The notice names the contract, so several empty attempts in one run are each shown.

## Consequences

- An attempt that leaves a declared write target as nothing but a placeholder body is refused
  and can be retried with a fresh worktree. A real edit to the file, even a small one, still
  clears it.
- A legitimately unchanged, non-placeholder write target still passes exactly as before, now
  with a notice that nothing changed.
- An attempt with a commit is refused too when a declared write target it left untouched held
  only a placeholder at dispatch. The dispatch content is the target's blob at the base
  commit, read from the repository, never from the worker's tree.
- Only Rust is recognized; a placeholder in another language is not covered by this decision.

## Alternatives considered

- **No change.** Leaves the failure mode open on any contract with a placeholder a weak
  attempt leaves untouched.
- **Refuse every empty attempt outright.** Closes the gap completely but breaks every
  legitimate no-op the existing policy exists for.
- **Parse a check command's own output for a "no tests ran" signal.** Would catch more cases
  but needs a parser per test runner and output format, and nothing to check for a project
  with no tests at all.

**Amended 2026-09-29: a struct and its placeholder trait impls are a stub.** A file whose only
items are plain structs, `use`, `mod`, `type` and trait impls whose bodies are all `todo!()` or
`unimplemented!()` is a placeholder: the shape an abandoned stub takes. An enum, a constant, a
macro call, an inherent impl, a trait definition, any item under a `cfg` attribute, and a
struct with a non-standard derive or attribute (an error or serde derive) each mark the file
as finished code, so error types with derived traits, platform shims and mocks written next to
real items are never flagged. The standard derives (`Debug`, `Clone`, `Copy`, `Default`,
`PartialEq`, `Eq`, `Hash`, `PartialOrd`, `Ord`) and documentation, lint and layout attributes
do not. Tests cover the shape both ways.

## Amendment: an attempt that changed none of its declared files is refused

A check that already passes before the work (`cargo build --workspace` over a stub the
placeholder check does not recognize) accepted attempts that changed nothing, and the run
counted those contracts done.

- An attempt with no commit (nothing differs from its dispatch base) on a contract that
  declared write targets now fails verification, whatever its check returned, and says why.
  The reason reaches the next attempt's brief; past the failure limit the contract is held.
  This is the rule an empty remediation already follows (0018).
- Still passed with the notice: a contract with no declared write (a check-only contract), and
  a handoff child adopting its parent's worktree.
- A contract whose work is already present now fails loud instead of passing silently.
