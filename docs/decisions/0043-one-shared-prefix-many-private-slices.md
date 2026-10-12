# Decision 0043: Workers share one byte-identical prefix and read private slices after it

**Status:** Accepted
**Related:** decisions 0006, 0008, 0012, 0025, 0042

## Decision

Every worker's request is assembled in two parts, in this order:

1. A **shared prefix**, byte-identical across every worker on the same vendor, model, and
   protocol: the system prompt, the tool definitions, and a **context pool** of slices that
   are broadly useful across workers. It ends at a cache breakpoint where the protocol has
   one.
2. A **private remainder**: this worker's brief, its own graph slices, its transcript.
   Nothing here is shared and nothing here is expected to cache.

**Nothing worker-specific may appear before the breakpoint.** No worker id, no timestamp, no
brief, no per-worker ordering. A single byte of variation moves the prefix into a private
cache entry and removes the entire benefit.

Pool membership (which slices are broadly useful enough to include) is chosen per dispatch
and recorded, rather than fixed.

## Context

Provider prompt caches are keyed on an exact token prefix, not on a conversation. Two
unrelated requests that open with identical bytes hit the same cache entry.

Under a per-worker prefix, N workers pay N cache writes, each amortized over that worker's
few turns. Under one shared prefix, N workers pay one cache write between them and every
subsequent worker reads. Fan-out into many short-lived workers therefore is not the worst
shape for prompt caching once the prefix is understood to be shared by bytes rather than by
conversation.

A shared prefix must be identical for everyone; a graph slice is per-symbol and therefore
per-worker. The two cannot occupy the same region of the request. Broadly useful context
that changes rarely goes in the pool, before the breakpoint, and is paid for once.
Task-specific context goes after the breakpoint, is paid for per worker, and should be kept
as small as possible. A slice in the pool that only one worker needs is waste charged to
every worker; a widely needed slice left out of the pool is paid for repeatedly. A worker
that needs something outside its slice reads it from the graph directly, and that read lands
in the private remainder.

**Limits.** This does not reduce the number of tokens processed; it reduces the price of
those tokens, at whatever discount the provider offers on a cache read, and even that price
reduction is not guaranteed until measured. It does not survive fan-out across vendors: a
cache entry belongs to one vendor's endpoint and one model, so a fleet spread across several
vendors shares each prefix only within its group, and the saving scales with the largest
same-vendor group rather than the whole fleet. This must never override vendor-fitness
routing: a cheaper wrong model is not a bargain. It is bounded by the cache's expiry, on the
order of minutes unless a longer duration is available and honored, so workers share a
prefix only if dispatched inside that window. It also depends on a capability not every
provider has: some protocols expose explicit cache breakpoints, some cache automatically with
no breakpoint field, and at least one has no caching concept at all. Where breakpoints are
unavailable, the discipline still likely helps, because automatic caching also keys on prefix
identity, but this is unverified.

## Alternatives considered

**Per-worker prefixes.** Discards a real price reduction once the prefix is understood to be
shared by bytes.

**One long-lived worker instead of fan-out.** The honest alternative; whether sequential
specialization wins over fan-out is a separate, open question.

**A shared conversation that workers append to.** Concurrent workers writing one transcript
is shared mutable state with a high measured conflict rate, and it destroys the append-only
property a transcript needs, since editing an earlier message invalidates later reasoning
tied to it.

**Put every slice in the pool.** The pool is charged to every worker, so a slice one worker
needs becomes many copies of waste, and it inflates every worker's context.

**One shared prefix plus private slices.** Chosen.

## Consequences

The prefix's byte-identical property needs an automated check, not just a comment: two
workers dispatched to the same vendor and model must produce identical bytes up to the
breakpoint, and the check must fail if a worker id is interpolated into the system prompt.

Cache reads and writes are counted separately, so the effect is visible in usage records with
no new instrumentation: a fleet sharing a prefix shows one worker with a large cache write and
the rest with large cache reads. If it shows many large writes instead, the invariant is
broken.

Pool membership is recorded per dispatch, so a later measurement can attribute a change in
cost to a change in the pool.

A cheaper prefix invites a larger pool, and a larger pool is charged to every worker, so the
right measurement is cost per successful task, not cost per cached token.

**Reactivation trigger.** A finding that sequential specialization outperforms fan-out, which
would remove the fan-out this optimizes. Or a measured finding that grouping workers by
vendor for cache sharing degrades task success, in which case vendor fitness wins and the
saving is abandoned.


## Amendment (2026-10-06): a worker's slices come from its own tree; an unreadable slice is left out, not fatal

A contract's attempts could fail at spawn, before any model call, with "cannot render slice
for node ...: cannot read `<file>`". The private remainder was read from the graph store's own
trees (a file's recorded source root, then the user's checkout), never from the worker's
worktree, and one node that could not be read failed the whole rendering. The operating
system's reason was not part of the message.

1. **The worker's own tree.** The private remainder is read from the worker's worktree at its
   dispatch base (`GraphSlices::with_tree`, `GraphStore::source_of_in`). Only that tree is
   read: a file absent from it, or holding another version than the indexed one, is not
   replaced by the checkout's copy, which can hold code the worker does not have. A tree that
   no longer exists (the probe worktree a run's routes are built in) says nothing about a
   file, and the store's reads apply. The `graph_slice` tool reads the worker's tree first and
   the indexed copy when the worker has changed the file.
2. **Left out, not fatal.** A slice whose file cannot be read for the worker, or whose node the
   graph no longer holds, is left out (`SliceLeftOut`: node, file, reason). The worker starts;
   its opening ends with a `slices left out` block naming each one; the `Context` event and the
   worker's result carry the list; the orchestrator traces one `notice` per slice and counts
   them on the report's `slices` line. A read error carries its cause. Only the graph's
   database failing ends the rendering. A brief that names slices with no graph configured is
   still refused.
3. **Dead source roots are repaired.** The message bus indexes a conversation file from a
   worktree it removes as soon as the file is committed, so the recorded root named a tree
   that was gone and every reader without a worktree of its own got "cannot read". After the
   commit the file is indexed again from the integration view, and after every integration
   `GraphStore::repair_source_roots` points each record whose tree no longer holds the file's
   indexed content at the view, when the view holds that content (or clears it when the
   checkout does). A view made again after git could not move it is repaired the same way.
4. **Sizing.** A hand-back child's slice cap counts a slice it cannot render from here at the
   per-slice bound and keeps it, so the spawn, not the sizing, is where a left-out slice is
   named.

## Amendment (2026-10-11): no graph for the worker is left out too, not refused

Item 2 said a brief that names slices with no graph configured is still refused. That is
replaced. A worker with no graph (`NoSlices`) leaves every node its brief names out, with the
reason "no graph is configured for this worker", and starts; its opening, the `Context` event,
its result and the orchestrator's notices say so, as for any left-out slice, and it reads the
files with its tools. Seen live: a re-planned contract named a node from the run's graph while
its attempts were assembled with no graph, and both died at spawn before any model call. The
source of that mismatch is also closed: when the build found nothing to index, an attempt now
opens the graph at assembly if it holds nodes by then, since the run indexes its own writes as
contracts finish.

## Amendment (2026-10-11): a prefix that really changed replaces the old one, said

Seen live: after a re-plan, the first worker spawned and the next two died at spawn with
"already has prefix ...; a different prefix ... must replace it explicitly". The cause was not a
worker-specific byte. The graph tool is listed only while an attempt holds a graph, and the
graph appeared mid-run (the run indexes its own writes), so the tool list, and so the shared
prefix, differed between two attempts of one run. That is a real change of the shared content.
The assembler now replaces the run's prefix for the vendor and model with the new attempt's
(`PrefixRegistry::replace`), which records an invalidation, and the attempt's start puts a trace
notice naming vendor, model, old digest, new digest and the cause. The first request on the new
prefix is a cache miss, and that is said. The refusal at spawn is unchanged: an environment
built outside the assembler, or a prefix that disagrees without a replacement, still fails
loud, as does a worker-specific byte (`refuse_worker_specific`).

Tests: `a_graph_that_appears_mid_run_replaces_the_shared_prefix_and_says_so` (host),
`a_replaced_shared_prefix_is_said_in_the_trace` (orchestrator).

## Amendment (2026-10-12): a slice name takes the forms a planner writes

Internal test runs traced 300 empty graph lookups. The common ones were
`path::Name` (`iface/src/lib.rs::Order`), `Type::method`
(`Discount::discount_cents`) and a bare file stem (`lib`). The orchestrator then gave the worker
whole declared files in place of slices, and workers called `graph_slice` 26 times against 2,512
file reads. Rust-path lookups (`Type::method`, a crate or module prefix) already resolved; the
misses were the forms below.

A name the graph does not know is now tried as: `path::Name` and `path::Type::method` (the symbol
defined in that file; a path part such as `src/lib.rs` may match several files); `Trait::method`
for a method declared without a body, which the graph holds inside the trait node, so the trait
is the result; a bare file stem, when one file has it; and a crate prefix that is the package name
in the crate's manifest. Rules: a type and its `impl` block count as one node; a prefix that names no
part of the symbol's file matches nothing; a name that matches more than one node gives none, the
miss stays an `empty_graph_lookup`, and a notice lists the candidates. Names that resolved before
resolve as before. The `graph_slice` tool takes the same forms and refuses an ambiguous one with the
candidates.

Not changed: a symbol the task has yet to create is still a miss, and that is correct.

Tests: `resolve_file_forms.rs` (graph), `a_path_and_symbol_name_resolves_without_the_fallback` and
`a_stem_shared_by_two_files_is_a_miss_that_names_both` (orchestrator).
