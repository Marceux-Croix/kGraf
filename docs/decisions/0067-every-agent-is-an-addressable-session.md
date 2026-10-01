# Decision 0067: Every agent is an addressable session; subagents, connections and a per-machine parallelism limit

**Status:** Accepted
**Supersedes:** decision 0017's starting concurrency numbers and its fixed maximums
**Amends:** decisions 0004, 0005, 0033, 0034
**Related:** decisions 0009, 0012, 0014, 0026, 0027, 0028, 0043, 0048, 0056, 0057, 0060, 0063

## Context

kGraf's orchestrator directs a set of worker agents, each attached to one piece of work. Before
this decision, a worker was reachable only by giving it a fresh task; a user could not send it
a note mid-task, and a worker could not create a helper of its own. The orchestrator was the
only addressable party, which meant a small correction had to be routed through a full new
task rather than delivered directly, and there was no way to draw a nested helper on the
graph. Concurrency limits were also fixed numbers chosen without measurement, rather than
values derived from the machine and the model providers actually in use.

## Decision

**Every agent is a session.** The orchestrator, each worker attempt, and each subagent it
spawns is a session with an id, a transcript, and a live view. One worker still serves one
piece of work and then retires; its transcript is kept.

**A user can act on one session directly**, as the exception rather than the normal route: a
note, a redirect, an answer to a question, a stop, or a resume. The orchestrator remains the
normal route for directing work; a direct message is also recorded as data the orchestrator's
next turn reads, so its own account of the run stays complete.

**Steering is built.** A message to a running agent is saved durably before it is
acknowledged, then delivered at the next safe point in that agent's loop. An interrupt mode
exists too: it can abandon a model call already in flight, close out any open tool calls with
a synthetic result, and then deliver the message. Nothing already written to the transcript is
ever rewritten; a message is always appended.

**An agent can ask the user a question directly**, pausing the same way an approval request
already does. If nobody answers within a scheduled run's time limit, the worker is told plainly
that no answer arrived and asked to state its own assumption instead. This outcome is always
recorded, never left silent.

**Agents may spawn subagents.** A subagent is a session with a parent, and it takes one slot
against the same parallelism limit any other agent does; if no slot is free, the request is
refused immediately with its reason, rather than leaving the parent waiting indefinitely. A
parent's stop cascades to its children. Two kinds exist:

- **Read-only subagents** share the parent's working copy for reading, and any write is
  refused by a fence around a deliberately empty writable set, not by removing a tool.
- **Writer subagents** work in their own copy, branched from a checkpoint of the parent's, with
  a write scope that is a subset of the parent's own declared writes. Their finished work is
  merged back into the parent's copy when they complete. No two live agents ever write the
  same path at the same time.

**The orchestrator approves connections between agents, not individual messages.** A
connection is a standing permission for two agents to exchange messages freely. Some
connections are approved automatically by rule (a parent and its child, a declared dependency,
siblings under one parent, a shared file both have read); any other pair needs the
orchestrator's approval. A message to an agent with no open connection is held while a request
is raised, never silently dropped. There is no hard cap on how many agents one agent may
connect to; an unusually high rate of connections or messages raises a visible notice and
slows delivery rather than refusing it outright.

**Parallelism is one setting, with a maximum computed from the machine and the configured
model providers, not a fixed number.** The limit is the smallest of: what the configured model
providers' own rate limits can sustain; available memory; available file descriptors. Cores
do not limit how many agents may be waiting at once, since an agent waiting on a model call
costs almost nothing locally; instead, a separate pool of execution slots, sized to available
cores, gates how many shell commands or builds can run at the same time. The computed maximum
is re-measured periodically and after certain events (a rate-limit response from a provider,
a change to the configured routes, opening or closing a project), and a user-set value below
that maximum is always honored; a value above it is capped, visibly.

**Reasoning visibility is stated, not assumed.** The application shows, per configured model,
whether its reasoning is returned in full, only as a summary, or not at all, both as the
provider's stated capability and as what has actually been observed on live calls. A model's
own reasoning is never paraphrased or sent to a different model; only its own final,
already-composed answer crosses between agents (see decision 0005).

## Consequences

- A spawn requested past the current limit is refused before it starts, and a limit lowered
  mid-run never kills an agent already running.
- Fan-out that used to be blocked by a fixed cap is now paced instead: too many connections or
  messages in a short window slows delivery and raises a notice, but never drops a message.
- Every direct message, subagent spawn, and connection request is recorded in the run's
  history, so an agent's full set of interactions is reconstructable after the fact.
- The user-facing "how much can run at once" control changes shape: it is now a single slider
  bounded by a computed, explained maximum, rather than two independent fixed limits.

## Alternatives considered

- **Give every agent its own operating-system process.** Rejected: it breaks the shared
  in-memory accounting every provider call must go through, and the shared prefix mechanism
  that keeps repeated context cheap to send.
- **Keep workers unaddressable and relay every correction through the orchestrator.** Rejected:
  it pays for a full extra round trip for what is often a one-line correction.
- **Inject a steering message by rewriting the agent's already-sent instructions.** Rejected:
  it breaks the append-only transcript and moves the agent off a cached, previously-sent
  prefix, which is expensive to rebuild.
- **Give subagents a reduced tool list instead of a write fence.** Rejected: a different tool
  list is a different prefix for that agent, which forces its own separate cache entry instead
  of sharing the parent's.
- **A fixed slider maximum, even a generous one.** Rejected: it ignores real provider rate
  limits, and it ignores machines with little memory or disk.

## Amendment (2026-10-01): a compaction replaces a worker's private remainder once

Decision 0095. The cache argument of this decision (the previous request's messages are a
byte prefix of the next request's) has one exception: when the runtime compacts a worker,
its next request carries the opening message plus the compaction notice and summary instead
of the earlier turns. The shared prefix is unchanged, so the prefix digest and the prefix
check still hold; only the worker's private remainder starts over, once. The `Compacted`
event is appended between rounds, like an injection, and refused mid-stream or
mid-tool-round; everything before it stays in the transcript. A user's request to compact
one session travels on the same queue as steering (`Injector::request_compaction`) and is
taken at the next boundary; a session that has compacted refuses a second request.

## Amendment (2026-10-01): thinking text sent as `reasoning`

The table above names `reasoning_content` as the Chat Completions field. Servers disagree on
the name: Ollama, OpenRouter and recent vLLM send `reasoning` (streamed `delta.reasoning`,
buffered `message.reasoning`). The chat adapter reads both; when both are present,
`reasoning_content` is taken and the text is carried once. Before this, a thinking model on
Ollama had its tokens counted and its text dropped. The `ollama` catalog entry now declares
full visibility, from a recorded live stream and a recorded buffered answer.
