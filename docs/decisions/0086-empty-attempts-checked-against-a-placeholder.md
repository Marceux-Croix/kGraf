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
   whitespace are stripped, is a single call to a standard "not implemented yet" marker.
   Only Rust source files are recognized today. A file with no function bodies, or with even
   one real one, is never flagged, so a legitimately unchanged file is never mistaken for an
   abandoned placeholder.
2. Every attempt accepted with no change now carries a notice saying so, alongside anything
   its check itself reported. This uses the run's existing notice path, so it reaches the same
   output a run's other notices already reach.

## Consequences

- An attempt that leaves a declared write target as nothing but a placeholder body is refused
  and can be retried with a fresh worktree. A real edit to the file, even a small one, still
  clears it.
- A legitimately unchanged, non-placeholder write target still passes exactly as before, now
  with a notice that nothing changed.
- Not covered: a placeholder left untouched inside an attempt that changed some of a
  contract's other declared files. Only a fully unchanged attempt is checked by this decision.
- Only Rust is recognized; a placeholder in another language is not covered by this decision.

## Alternatives considered

- **No change.** Leaves the failure mode open on any contract with a placeholder a weak
  attempt leaves untouched.
- **Refuse every empty attempt outright.** Closes the gap completely but breaks every
  legitimate no-op the existing policy exists for.
- **Parse a check command's own output for a "no tests ran" signal.** Would catch more cases
  but needs a parser per test runner and output format, and nothing to check for a project
  with no tests at all.
