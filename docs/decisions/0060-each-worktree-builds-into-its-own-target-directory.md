# Decision 0060: Each worktree builds into its own target directory, seeded by a clone

**Status:** Accepted, from measurement

This record exists so a shared build directory is not reintroduced later for speed.

## Decision

Each worker's git worktree builds into its own Rust build output directory. A new worktree's
build directory is seeded with a copy-on-write clone of an already-warm build directory (using
the filesystem's native clone support where available, or a plain copy otherwise, with the
method used reported). Worktrees never share a single build output directory.

## Context

Measured on this project's own codebase, across several hundred build units, under heavy
concurrent machine load, so the figures below are noisy:

| Case | Wall time |
|---|---|
| Own target, cold | 27.6 s |
| Shared target, later worktree | 9.0 s |
| Own target seeded by a filesystem clone (1.7 s), then built | 5.3 s |
| Two concurrent builds, shared target | 34.3 s (serialized on the build lock) |
| Two concurrent builds, own targets | 55.0 s |

A shared build output directory produced silently wrong code: the build tool judges freshness
by file modification time, so a worktree built after another worktree compiled nothing and
received the other worktree's code instead of its own. That is a correctness failure, not
merely a speed trade-off.

## Alternatives considered

- **A single shared build output directory.** Fastest for a second worktree in isolation, but
  silently produces wrong code, and concurrent builds serialize on the build tool's own lock.
- **A fully cold, independent target directory per worktree.** Correct, but costs roughly 28
  seconds per worktree.
- **An independent target directory seeded by a filesystem clone.** Chosen. Correct and costs
  roughly 5 seconds per worktree.
- **A build cache tool.** Not evaluated yet; to be measured if cold builds come to dominate
  overall time.

## Consequences

Disk use is copy-on-write until a worktree's build genuinely diverges from the source it was
seeded from. The seeding source is kept warm by continuously building the project's own
integration branch.

Reactivated if the underlying build tool gains content-hash-based freshness checking instead
of relying on modification time.
