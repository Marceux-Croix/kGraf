# Decision 0005: An internal Anthropic-shaped envelope, with no agent wire protocol in v1

**Status:** Accepted
**Amended by:** 0051 (hosted tool servers), 0067
**Related:** decisions 0001, 0006

## Context

### Why Anthropic-shaped

Not because the shape is neutral. Because the Anthropic Messages API has
become a de facto second wire standard. Anthropic, Moonshot Kimi, DeepSeek,
Qwen, and OpenRouter all expose it, and Kimi's endpoint mirrors Anthropic's
usage object field for field including both cache TTL buckets. For two of
the highest-value providers the envelope is not an abstraction at all; it is
the wire format.

OpenAI and Google do not expose it, confirmed by probe, and need real
adapters. That is acceptable.

### Why reasoning cannot be normalized

An OpenAI function call carries both an id and a call id where an Anthropic
tool use has one. Reasoning items are separate output entries with their
own id and encrypted content. Gemini is a third shape again. Any mapping
loses an identifier and corrupts replay.

Worse, Anthropic thinking blocks are **prefix-bound**: a block stays valid
only while the system prompt, the tools, and every preceding message are
byte-unchanged, and a mismatch is a hard 400 error. Switching models
mid-conversation silently drops blocks the new model cannot read.

For a harness whose premise is routing work across providers and models,
this is the most dangerous behavior in the system, and it forces the design
constraints recorded in Consequences below.

### Why no agent wire protocol in v1

Every agent is a task inside one Rust process (decision 0001). Agents
sharing an address space pay nothing to pass a reference.

Agent2Agent (A2A) between them would buy JSON-RPC framing that is
immediately deserialized back, an agent card served over a URL namespace
that does not exist, an auth model whose threat cannot occur inside one
process, and an unreachable auth-required state. Meanwhile A2A's Rust
binding is pre-1.0 and has roughly two orders of magnitude fewer downloads
than the Model Context Protocol (MCP) Rust binding `rmcp`. Adopting it means
taking on the least mature dependency in the stack to solve a problem kGraf
does not have.

No standard exists for in-process multi-agent coordination. That was looked
for specifically and none was found, which argues for defining an envelope
rather than against it.

### Why MCP as a client only, and not as agent-to-agent

MCP's current revision moves **away** from agent-to-agent, not toward it.
Sampling, roots, and logging are deprecated, with the suggested migration
for sampling being to integrate directly with provider APIs. It is now
stateless, and servers can no longer initiate requests. Transports are
stdio and streamable HTTP only, and auth is optional with stdio
implementations advised not to use it.

As a tool client over stdio it needs no socket, no port, and no auth, which
was verified by compiling and running `rmcp` against a real third-party
server.

## Decision

kGraf defines its own internal message envelope, shaped like the Anthropic
Messages API, as Rust types it owns. Providers are adapted onto it.

Reasoning and thinking content is **never normalized**. It is carried as an
opaque, provider-tagged blob attached to the turn and replayed verbatim to
the same provider and model, or not at all.

For agent-to-agent messaging, kGraf defines its own internal envelope and
ships **no wire protocol and no HTTP server** in v1. The envelope's data
model mirrors A2A's so that an external shim later is a serialization layer
rather than a refactor.

MCP is adopted **as a client only**, over stdio.

## Consequences

**Mirror A2A's vocabulary in the Rust types** so a future shim is a pure
mapping: a task with id, context id, status, artifacts, history, and
metadata; a message with role and ordered parts; a unified part enum; an
artifact type distinct from a message; and a capability descriptor keyed on
skills.

**Keep A2A's eight task states exactly, including the unreachable
auth-required one.** A variant costs nothing now and its absence later is a
breaking change. Keep their single-L spelling of `canceled` so the shim is a
pure mapping.

**Thinking blocks are not portable context.** A worker's thinking blocks are
never handed to a worker on a different model.

**Transcript history is append-only per agent.** Editing an earlier message
to compact or redact invalidates everything after it. Compaction must use
documented escape hatches, not history rewriting.

**Keep both tool-call id slots and keep the content block array ordered**,
or round-tripping to OpenAI loses an identifier.

**Use an untagged passthrough variant for unknown content blocks** so a new
block type round-trips byte-identically. The serde `other` attribute parses
and discards, which would silently corrupt replay. This was verified in a
working crate.

**Target the MCP revision `rmcp` actually negotiates**, which is still the
previous one despite a newer revision being defined in the same crate.

**Reactivation trigger for an A2A shim:** the first time a process kGraf
does not control needs to address a kGraf worker. Keep the mapping in one
module. Also watch the MCP Agents working group: if it lands an agents
extension the two-protocol question may collapse, and kGraf wants to have
bet on neither wire format.

## Alternatives considered

**Adopt A2A as the internal protocol.** Rejected: pre-1.0 Rust binding, and
all of its value is in a problem kGraf does not have.

**Adopt MCP for agent-to-agent.** Rejected: the spec is actively narrowing
away from that shape, and building on sampling or roots would be dead code
within a year.

**AGNTCY.** Rejected outright. Roughly three orders of magnitude fewer
monthly downloads than A2A, and its own connect protocol was archived.

**ACP.** Not an option; it merged into A2A and its repository is archived.
Note three different things are called ACP, only one of which is alive and
it is a payments protocol.

**Normalize reasoning into one shape.** Rejected on the identifier-loss and
prefix-binding evidence above.

**Own envelope mirroring A2A's data model.** Chosen.
