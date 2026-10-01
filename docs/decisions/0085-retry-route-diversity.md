# Decision 0085: A retry of an unpinned contract uses a different route, and a recorded route is not a pin

**Status:** Accepted
**Related:** decisions 0018, 0073, 0075, 0083

## Context

When a contract's attempt fails, the orchestrator dispatches it again. For an unpinned
contract, the retry should be free to use a different route: decision 0075 already rotates
routes under the `rotation` policy and escalates to a better-measured route under `fitness`,
for any attempt index other than 0.

That never happened. On its first dispatch, `try_dispatch` wrote the chosen route into
`contract.route`, so that a handoff child can read its parent's route later. After a failed
attempt, `worker_routes` is cleared, and `route_base` then fell back to `contract.route`. It
could not tell that recorded route from a real pin. Each retry also started a fresh flight at
attempt index 0, the one index that never rotates or escalates. The result: every retry of
an unpinned contract ran on the route that had just failed it, under both routing policies.
A gate remediation contract could repeat the same failure on the same route until its
retries ran out.

## Decision

1. On a fresh flight of an unpinned contract, `try_dispatch` passes `contract.failures + i`
   as the attempt index to `attempt_route`, where `i` is the parallel-attempt index. The
   `contract.failures` counter already exists; no new counter is added.
2. `OpenContract` gets a field `route_pinned: bool` (default `false`, `#[serde(default)]`).
   `route_base` falls back to `contract.route`, and `try_dispatch` skips the offset, only
   when `route_pinned` is `true`. It is set `true` only where a route is a real pin: an
   explicit route in the plan (`contract_from_spec`), or a conflict resolver's inherited
   route (decision 0073). A route written at dispatch time for bookkeeping leaves it `false`.

## Consequences

- A retry of an unpinned contract, including a gate remediation, runs on a different route
  from the failed attempt, under both routing policies. Tests in
  `crates/kgraf-orchestrator/tests/retry_route_diversity.rs` cover each policy.
- A real pin is unchanged: its retries use exactly the pinned route.
- A saved run-state record from an earlier version still loads. The new field defaults to
  `false`, which is correct for every existing record.

## Alternatives considered

- **Always offset retries by `contract.failures`.** Rejected: it would silently replace a
  real pin on retry.
- **Clear `contract.route` on every failure.** Rejected: the same field holds an explicit
  plan pin, which would then be dropped after one failed attempt.
- **Change what attempt index 0 means inside `attempt_route`.** Rejected: other callers
  depend on index 0 returning the base route unchanged.

## Amendment (2026-10-01): plan intake repairs what the planner's intent makes certain

A re-ask of the planner costs minutes on a local model. Three mistakes are now repaired at
intake, each stated as a run notice, and one refusal is reworded:

- A contract's `route` that is not a configured route but is a task-kind word
  (`implementation`, `tests`, ...) is taken as unset, and becomes the contract's `task_kind`
  when that is absent. A route name that is configured is a pin whatever it is called; any
  other unknown name is still refused. The decomposition prompt now says a kind goes in
  `task_kind` only and `route` is null unless the Routes line lists it.
- An absolute write path inside the repository root becomes the repository-relative path.
  One outside the root, or one that leaves it with `..`, is still refused.
- A write to protected state (`.kgraf`, `.git`, ...) or to Cargo or rustup configuration is
  refused with a reason that names it, not as "not a relative path inside the repository".

Tests: `a_task_kind_word_in_route_is_left_unset_with_a_notice`,
`an_absolute_write_inside_the_repository_is_made_relative_with_a_notice`,
`a_protected_write_is_refused_with_its_own_reason`.
