# Decision 0001: kGraf implements its own agent loop against provider APIs

**Status:** Accepted
**Related:** decision 0002

## Context

Three properties the product depends on all require owning the agent loop
rather than wrapping an existing vendor CLI or agent runner.

**Token accounting.** The user is promised a live per-agent and per-task cost
readout on the graph. A wrapped CLI reports, at best, a summary after the
fact, and often nothing structured at all. There is no way to attribute
spend to a node or a brief from outside the process making the calls.

**Context assembly.** The central idea is that agents read graph slices
rather than whole files. A vendor CLI manages its own context window by its
own rules and will happily read whole files. Handing it a slice does not
stop it from ignoring the slice.

**Provider mixing per worker.** The requirement is that one job can run one
model for scripting alongside a stronger model for architecture, and that
they can address one another. Vendor CLIs configure their provider from
process environment, which cannot be varied per subagent within one process.
Wrapping them means one process per worker, which turns a cheap in-process
task into an expensive subprocess with a file-based messaging hack between
them.

## Decision

Worker agents are tasks inside the kGraf process, driven by a loop written
in Rust directly against provider HTTP APIs. kGraf does not wrap any vendor
agent CLI, agent SDK, or other vendor agent runner.

Each worker holds its own client configuration: provider, model,
credentials, base URL, sampling parameters. Nothing about a worker's
provider is inherited from process-level state.

## Consequences

kGraf owns tool definitions, tool dispatch, retry and backoff, streaming,
cancellation, and context assembly. This is a substantial amount of
implementation work, since it reimplements what vendor agent runners already
solved.

Provider-specific behavior is confined to adapter crates behind one trait.
Anything that leaks provider detail into the core is a defect.

Every provider call routes through the accounting layer. A code path that
can reach a provider uncounted is a bug.

The constraint that vendor CLIs cannot scope provider configuration per
subagent does not apply to kGraf and should not be cited as a limitation in
its design.

**Reactivation trigger:** a vendor publishes an embeddable agent runtime that
exposes per-call token usage including cache reads and writes, accepts
externally assembled context without overriding it, and allows per-instance
provider configuration. That would make wrapping a vendor runtime worth
reopening in a superseding record.

## Alternatives considered

**Wrap existing CLI agents as subprocesses.** Fastest route to something
that appears to work, and inherits mature tool implementations and
permission prompts for free. Rejected: it forfeits all three properties
above. Token accounting becomes an estimate, context control becomes
advisory, and cross-provider messaging becomes files on disk between opaque
processes. The graph would be showing a guess at what the agents are doing.

**Hybrid: native loop with a subprocess adapter as one provider type.**
Attractive as a compatibility story. Rejected for v1 as premature interface
surface. The adapter would be unable to satisfy the accounting and context
contracts that the native path enforces, so it would either weaken those
contracts for everyone or exist as a permanently second-class path. It can
be revisited once the native contracts are stable, by a superseding record.

**Native loop against provider APIs.** Chosen. Most work, and it means
reimplementing tool dispatch, retries, streaming, and cancellation that
vendor runners already solved. Accepted as the cost of the product actually
being what it claims.
