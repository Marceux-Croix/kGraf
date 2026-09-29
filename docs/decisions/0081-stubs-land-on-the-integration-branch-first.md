# Decision 0081: An interface contract's placeholder implementation is committed onto the shared branch before any party is dispatched

**Status:** Accepted
**Related:** decisions 0018, 0023, 0026, 0027, 0030, 0031, 0076

## Context

kGraf already proves an interface is soundly checkable before either its producer or its
consumer is dispatched: the consumer's expectations and its test are proven to compile against
a trivial placeholder of the producer's side, and proven to fail against that placeholder and
pass against a correct reference. That proves the interface itself is sound. It does not
touch what each worker actually starts from: a worker dispatched after these checks pass still
branched from whatever the shared integration branch happened to already contain, which had
never actually seen the placeholder. A consumer's first real look at the producer's interface
was still the contract's written brief, not code its own tools had already accepted.

## Decision

**Once an interface contract's checks pass, and before either party of it is dispatched, its
placeholder implementation is committed onto the run's own shared integration branch**, one
commit per version of the interface. Every working copy created after this point, for either
party, branches from a point that already compiles against the interface, not from a brief
describing one.

**The placeholder lands through kGraf's existing merge-back path, never a separate git
mechanism of its own.** This means a landed placeholder gets every safety property a worker's
own finished attempt already gets: attributes are read only from the trusted, already-merged
tree, git hooks and filesystem watchers are disabled for every git process involved, and an
attempt touching a protected path (git's own configuration, or kGraf's own state) is refused
loudly, naming the path, rather than merged.

**A producer that simply leaves the placeholder in place, rather than replacing it with a real
implementation, is caught.** kGraf compares the producer's delivered content at each
placeholder's own path against the placeholder's own committed content, byte for byte; any
path still identical is named as left unimplemented.

## Consequences

- Every party of a checked interface contract dispatches into a working copy that already
  compiles against the interface, not a brief describing one.
- A placeholder commit is a real, ordinary commit in the run's own history, one per interface
  version, and reads no differently from a worker's own attempt in that history.
- A failure to land a placeholder (a refusal, a conflict, an error) is surfaced and holds the
  run, exactly like any other interface check failure, never silently skipped.

## Alternatives considered

- **Write the placeholder's tree directly, without a full working copy checkout.** Faster, but
  rejected: every one of merge-back's existing safety checks would need reimplementing and
  reverifying in a second place, and a future improvement to those checks would not
  automatically cover this second path.
- **Detect an unimplemented placeholder with a marker comment instead of byte comparison.**
  Rejected: a marker cannot be language-agnostic without a comment syntax table for every
  supported language, and adding it after the placeholder was already proven to compile would
  make the committed file differ from the one actually checked.
