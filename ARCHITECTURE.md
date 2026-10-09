# kGraf Architecture

## Overview

A local desktop harness where one orchestrator superagent directs worker agents
across multiple model providers, with a galaxy-style code knowledge graph as
the primary interface.

One Rust process does everything except draw. It:

- indexes the codebase into a graph;
- runs the orchestrator and every worker agent as concurrent tasks;
- talks to model providers through one client trait, and to MCP servers the user
  configured, local or hosted (decision 0051);
- streams events to the shell that renders the graph.

The shell is a native Rust window with its own taskbar icon and tray presence. It draws
with egui and `wgpu`, and runs without a GPU (decision 0054). kGraf is single-user and runs
entirely on the user's machine; only model and MCP calls leave it.

Sections below describe the built system. A section or feature marked **Not built yet**
is design only; nothing on the live path depends on it. The crate map after the
components names the crate that holds each part.

```
+--------------------------------------------------------------+
|  shell: native Rust window + tray, egui + wgpu (0054)        |
|  galaxy graph (GPU, or a stated CPU mode) | text surface |    |
|  orchestrator input box                                      |
+---------------------------^----------------------------------+
                            | typed event bus (core -> UI)
                            | commands (UI -> core)
+---------------------------v----------------------------------+
|  Rust core                                                   |
|                                                              |
|  orchestrator ---- briefs ----> agent runtime (N workers)    |
|       ^                              |        |              |
|       |                              |        +-> tools      |
|       +------- message bus ----------+                       |
|                                      |                       |
|  code graph  <---- slices -----------+---> MCP servers       |
|   + layout cache                     |     (local or hosted) |
|   + write-lease registry             |                       |
|       ^                              v                       |
|  indexer (tree-sitter)        provider layer                 |
|       ^                         |  ledger (accounting)      |
|       |                         v                            |
|  working tree + worktrees     Anthropic / Kimi / OpenAI /... |
|                                                              |
|  instructions + skills inventory (from the user's agents)    |
|  scheduler (recurring and one-shot runs)                     |
|                                                              |
|  SQLite: graph.db (nodes, edges, leases, work, meta,         |
|            change_log), ledger.db (calls), schedule.db       |
|  files:  .kgraf/runs/ (transcripts, traces),             |
|          docs/agent-comms/ (agent messages)                  |
+--------------------------------------------------------------+
```

## The governing principle

**The published results on multi-agent coding split on shared state versus
disjoint state, not on single agent versus multi-agent.**

Published results favorable to multi-agent work come from tasks that are
read-mostly or explicitly partitioned. Unfavorable results come from agents
writing into the same state, with measured cross-agent conflict rates around
42 percent and an uncoordinated pair-pass rate of 23 percent.

So kGraf is built so that **workers read widely and write disjointly**, and
every concurrent writer **declares its write scope before writing**. Decisions
0009 and 0012 carry the reasoning.

The primary mode is **division of labor**: different vendors working different
parts of one problem at the same time, cooperating. Redundant attempts with a
verifier selecting a winner are a secondary mode, not the default.

## Coordination and collision control

The design question behind this part of the system: could a code graph give
the orchestrator, ahead of dispatch, the coordination information that
unlinked parallel agents normally have to guess at, and usually guess wrong?

Testing that meant measuring whether the graph's call structure predicts
which changes collide. It does not, closely enough to matter: across many
repository configurations and two languages, whether the graph judged two
changes disjoint was close to statistically independent of whether they
actually collided. Most missed collisions involved files with no call-graph
path between them within a short number of hops, or files absent from the
graph entirely. The diagnosis: a call graph encodes what code **is**, which a
snapshot can derive; collision risk depends on what changes **with** what,
which only history can derive.

Decision 0020 made co-change (how often two files change together, from
version history) the collision predictor instead, in place of the call graph.
Co-change turned out to have no useful predictive frontier for scheduling
either: leasing files by co-change degree only ever serialized already-
contended pairs, never prevented an otherwise-missed collision. Decision 0022
therefore stopped predicting collisions altogether.

Collision control today is:

- **runtime write leases** (0048 to 0050): exact, taken when a write set is
  admitted, built;
- **quality gates** as work nodes (0018, 0019): built;
- **task-similarity scheduling preference** and **speculative merge, build and
  test** (0022): not built yet.

The code graph kept its other jobs regardless of its role in collision
prediction: producing context slices for agents, and modeling the interface
structure the orchestrator plans against.

## Components

### Code graph

The index that is simultaneously the picture the user looks at and the
context the agents read from. Every node is a code element.

**The graph indexes locations, not text.** A node carries a file path and a
line span; source is re-read from disk on demand. This keeps the index small
and makes it impossible to serve stale source.

Chunking follows codebase-memory-mcp: disjoint tree-sitter AST spans, with
node types named per language in a specification table, no sliding windows
and no overlap. Markdown headings become section nodes whose span is the
heading line, with the body captured as a property.

Its most load-bearing job is producing a **context slice**: a requested
symbol plus a bounded neighborhood sized to a token budget. An outside
60-query study on a roughly 200-file TypeScript repository measured 23
percent fewer tokens with slice retrieval than with whole-file reads. kGraf
has not reproduced that figure directly; its own tests check the direction of
the mechanism, not the magnitude. The known weakness of slice-based retrieval
is cross-cutting changes, where it loses recall exactly when a change spans
the codebase.

**A slice shows each line once.** A top-level symbol's enclosing element is its whole file,
which holds the symbol and every sibling, so a slice with room for the file shows the symbol,
then the file with the symbol's lines named in one line, and no sibling again; a callee or caller
inside the file is one line naming it. The slices of one brief share that record: a symbol an
earlier slice showed is one line naming it, and a file shown already is not shown again. A brief
of six slices of one Python module and its test file (3,413 bytes of source) rendered as 23,952
bytes before this rule and 5,555 after.

**Incremental reindexing compares content hashes, not just modification time
and size.** Comparing only time and size misses a change that preserves
both, and rapid agent writes are precisely that workload.

**An index write is one transaction, and a read never builds an index.** A file's content hash commits with its nodes, so a failure between the two cannot leave a file that reads as unchanged with stale or missing nodes. Only `kgraf index` creates or rebuilds the database. Every other command, and the host, opens an existing one, and stops with a message naming `kgraf index` when it is missing or was written by another schema version. A slice reads each node's lines from the file, compares the file's hash with the indexed one, and refuses an edited file. The store records a source root for each file, so code merged from a worker's worktree can be sliced before it reaches the checkout. A pass writes up to 256 files per transaction and rolls the open group back as a whole if it fails, because one commit per file rewrote the same hash-keyed index pages for every file and grew slower as the index grew. The cache holds up to 64 MiB. Call resolution groups each name's definitions by language family, file and directory once, so its work follows the number of call sites and not their product with the definitions of a common name like `new`.

**A walk leaves out installed packages, and parses on worker threads.** A directory that holds a `pyvenv.cfg` is a Python virtual environment. A walk skips it at any depth, whatever its name, lists it with the other excluded directories in the report, and the corpus reader skips it too. Two environments under a tooling directory were 15,000 of 20,000 files in one repository and most of its cold first reading. A pass stages files in windows of 256: the change decision and the writes stay on one thread in walk order, and the tree-sitter parse between them runs on up to four threads, so the rows and the change log do not depend on the thread count.

**Call resolution is name-based, scoped by proximity, and bounded in
fan-out.** An edge per same-named definition, with no bound, produced 6.8
million edges on a 1.39-million-line repository; an edge set that wide
carries no information a consumer can act on. Fan-out bounding is a measured
exception to the project's general rule of weighting signals rather than
thresholding them (decision 0038).

**Three indexer rules refine call resolution (decision 0065).**

- **Calls stay in their language family.** A candidate definition must be in
  the caller's language family (Rust; Python; TypeScript, TSX and JavaScript;
  C and C++; Go; Java; Ruby; C#; PHP) before proximity scoping applies. Call
  sites dropped by this gate are counted and reported by `kgraf index`.
- **Each extractor is stamped.** A per-language hash of the extractor
  version, the specification table, the call kinds, the tags query and the
  grammar's node kinds and fields is kept alongside the index. A changed
  stamp re-derives that language's files from unchanged bytes; nothing else
  is touched.
- **Two tiers of languages.** The full tier (Rust, Python, TypeScript, TSX,
  JavaScript, Markdown) uses kGraf's own extraction tables. The tags tier
  (Go, Java, C, C++, Ruby, C#, PHP) chunks from each grammar's own
  `TAGS_QUERY`; a slice marks its members as resolved by name only.

**Node identity is hierarchical and signature-derived (decisions 0049,
0050).** A node's id derives from its parent's id, its name, and a digest of
its signature, meaning the source up to the body. A body edit, or an edit
elsewhere in the file, does not change it. This replaced a positional
ordinal, which rebound ids to different code whenever a same-named sibling
was inserted. Siblings no signature can tell apart carry an explicit
"identity unstable" flag rather than a false claim of stability. Measured
over 449 commits of a reference project, line-range identity rebinds about 1
percent of unchanged elements to different code, positional ids rebind far
more often, and hierarchical ids never did.

### Co-change relation

A weighted relation computed by kGraf from version history, not consumed from
another tool. Decision 0020 made it the collision predictor after the call
graph failed in that role; decision 0022 later withdrew that role, because
raw co-change is not a sufficient predictor either. Today `kgraf cochange`
builds it and `kgraf partners` queries it; no dispatch decision consumes it.
It may still earn a place as a secondary scheduling signal.

**Every observed pair is kept with its co-change count as a weight. Nothing
is thresholded at index time**, because the correct operating point depends
on the consumer, and a value discarded at index time cannot be recovered.

Commit-size filtering (excluding very large commits, which create spurious
pairs across the whole tree) and recency weighting are read-time query
options, not index-time decisions.

Git is the only source today, read regardless of who authored the commits, so
a user switching between kGraf and another harness keeps the file-level
relation.

**Not built yet:**

- sub-file granularity for the co-change relation;
- recording kGraf's own observations from live runs (per-task write sets and
  observed collisions are richer than git, which structurally cannot contain
  a collision, since review resolves conflicts before they reach a commit);
- a cold-start policy composing a directory-proximity prior with a
  concurrency ramp. Runtime leases need no history, so this no longer serves
  a safety role; it would only be a scheduling refinement.

### Document ingestion and intent interpretation

Reads whatever documentation the project already has and turns it into a
normalized plan model. No format, filename or arrangement is required;
interpreting the corpus is kGraf's job, not the user's.

It emits planned components as graph nodes with no code behind them yet, work
items with their dependencies and acceptance criteria, constraints and
refusals the documents state, and the project's declared build and
validation commands. **Not built yet:** constraints from the corpus that bind
the orchestrator's own behavior.

That last output matters beyond seeding. **The corpus supplies the verifier**
that later validates integration, so a model's opinion never stands in for a
test. Where the corpus declares no gates, the orchestrator says so and asks,
rather than inventing a plausible command.

Interpretation is a model pass over prose and will sometimes be wrong, so the
orchestrator states its reading and takes correction before acting on it.
Re-interpretation is incremental, scoped by change detection.

`kgraf-ingest` parses Markdown, PDF and DOCX, and reads plain-text and source-format files (`.txt`, `.log`, `.json` and the like) up to 2 MiB; a larger one is reported as unreadable, not read. `kgraf-interpret` gathers the
corpus with the graph's incremental index, ranks it with aider's repo-map
PageRank (ported into `kgraf-graph`), builds a bounded brief, and makes one
accounted model call. The reading prints a plan id, and nothing is created in
the repository until the user approves that id. A command counts as
discovered only when a document contains it whole; a proposed command
becomes a gate only when the user accepts it by name. Doneness is derived
from code and recorded test passes, never from a document's claim alone.

The brief ranks documents by how many others name them, depth and planning words. The first
six sections of each planning document at the repository root (`ROADMAP.md`, `CLAUDE.md`,
`README.md`) are taken before all others, however many other documents exist. Nested copies
rank as ordinary documents.

**Presumed work nodes (decision 0064).** Before any plan is approved, the
project's own roadmap document is read into the graph as **presumed** work
nodes, so a fresh or half-built project shows what is done, in flight, and
still to do from the first index.

```text
  ROADMAP.md, TODO.md, PLAN.md ... roadmap parser (no model)-----+
                                                                  +--sync--> presumed work nodes
  the model's reading of a prose plan (one accounted call) ------+

  approved plan seeded --adopt--> the presumed nodes its items cite

  read time: claim (the document's status word) + named code in the graph
             -> presumed done / partly done / remaining, with any disagreement said
```

- **Deterministic first.** The roadmap parser reads status tables, task
  lists and status headings directly. A plan written only as prose, or an
  empty directory with an instruction, goes through the interpretation pass
  instead; its items are presumed too, unless a roadmap node already stands
  for the section they cite.
- **Store rules.** Ids come from the document's own structure. A re-read
  with nothing changed writes nothing. A changed entry is updated in place;
  an entry no longer listed is marked gone, never deleted. A node that real
  work has since adopted is left as it is.
- **Adoption keeps identity.** Seeding an approved plan marks each presumed
  node its items stand for as adopted by that item; the run record names its
  plan, and the galaxy draws the running work on that one piece of work.
- **Where it runs.** `kgraf index` runs it by default (skippable), as does
  `kgraf orchestrate --corpus` and `kgraf plan --from-roadmap`. The desktop
  shell runs the same read when it opens an already-indexed project: opening
  a project, switching to it, or restoring a saved workspace triggers a
  background read, once per open and again whenever a discovered roadmap
  document changes on disk. It never creates or rebuilds a project's graph: a
  folder with no index is left alone with a "not indexed" notice. This path
  makes no model call and spends no token.

### Provider layer

One Rust trait with adapters behind it, hand-rolled on `reqwest` and `serde`.
There is no official Anthropic Rust SDK, and each credible cross-provider
crate surveyed corrupts exactly the cache and usage data the cost readout
depends on.

The seam is a **small mandatory trait plus separate opt-in capability
traits**, never a wide interface with capability flags. Provider types never
escape the adapter crate.

Each worker holds its own configuration, so provider, model, credentials and
base URL vary per worker within the single process. Per-worker model
selection is built; sticky inherited fallback chains are not built yet.

**Accounting normalizes per protocol, not per provider**, because providers
split into additive and inclusive conventions for whether cache tokens sit
inside the input total, and one vendor ships both behind a single key. Every
call stores six independent usage fields plus the raw usage response
verbatim.

**A buffered call's time bound scales with its output cap.** A call that is not
streamed sends no byte until the whole answer exists, so the wait for the response
headers is the generation time. The bound is the larger of the configured timeout
(600 seconds by default) and the output cap divided by a floor rate of 20 tokens per
second (`ProviderConfig::floor_tokens_per_second`): 3,200 seconds at a 64,000-token
cap. A streamed call keeps the configured timeout, since its first byte does not wait
for the answer. The Chat Completions, Responses, Anthropic, Bedrock and Gemini adapters
all apply it.

**Every adapter asserts the response model equals the requested model.**
Testing against real vendors, rather than mocks, found three providers that
silently substitute or ignore the requested model while still returning
HTTP 200; the assertion catches this in the type system, since the mandatory
trait method takes a ledger-minted token with no public constructor, so an
uncounted provider call does not compile.

All three integrated protocols have exchanged a real request with a real
vendor. That live testing found defects a mock had passed, such as a
vendor's cache-write reports being discarded and its reasoning tokens being
counted as text; both are fixed, and each incident replays as a regression
test.

**Vendors live in a catalog, not in code** (decision 0041). The catalog holds
twenty-three routes, each with its wire protocol, usage convention, verified
capabilities and a confidence rating. "OpenAI-compatible" is treated as two
distinct protocols, Responses and Chat Completions.

**Models of every kind, and the cloud routes** (decisions 0089 to 0092). A route also has a
model kind, an open name checked against a registry: `generative` (prose and tool calls, the
default), `typed-decision` (a value inside a declared answer space, with a probability),
`embedding` and `rerank`. A kind is one row in each layer's table (the core registry, a call
trait, a fleet builder, a worker tool), so a new kind touches nothing else. Every call of every
kind takes a ledger-minted token and is priced where the price table has the model, and
reported as unpriced otherwise. Nothing calls a model of a non-generative kind on its own: the
decomposition request lists the run's routes by kind, and the orchestrator decides. Kinds never
cross: a call for a kind a route does not serve fails loud. The catalog also holds `gemini`
(native `generateContent`), `vertex-gemini` and `vertex-anthropic` (Vertex AI), `bedrock` (the
Converse operations, signed with SigV4), and `azure-openai` and `azure-openai-responses` (a
transport option of the two OpenAI adapters, with a configured deployment-to-model mapping). All
of these are built and tested against recorded replies only. `docs/MODEL-SUPPORT.md` lists every
route and kind as verified or built and untested.

**Credentials.** Each route reads its credential from its own environment
variable, shared only where one key genuinely works on several routes. A
missing key raises and names the variable.

**How a failed call closes.** A failed call is written off as unbilled only
when it provably billed nothing. Anything that reached the vendor stays
unreconciled, because vendors bill some failed attempts that ran.

**Streaming, the ledger and rate limits.** All adapters stream incrementally
over one shared decoder and stream guard; a truncated or failed stream
leaves the call unreconciled with its partial usage and request id. The
ledger is an event-sourced hash chain (decision 0059) with one writer thread
that commits in groups, plus an external tip anchor kept off the settle path.
Per-route rate limits use a GCRA (generic cell rate algorithm) admission
curve; a call that would wait past its limit fails immediately and its
ledger row closes as failed. The limiter is shared across every kGraf
process on one machine (decision 0072): its state is one SQLite file per
user, keyed by route and a hash of the credential, checked inside atomic
transactions, never a hand-rolled cross-process lock. The shared clock is read
inside the transaction: a read taken before the write lock let a clock reset
erase a newer reservation and admit too many calls. If the shared store
cannot be opened, the run refuses to start; there is no silent fallback to a
process-local limit.

### Agent runtime

Workers are tasks in this process, not subprocesses. The loop assembles
context, calls the provider, dispatches tool calls, iterates, and returns a
compact structured result.

Within a round, **each tool call waits for every earlier call whose read or
write access conflicts with its own**, so independent calls overlap and
conflicting calls run in the order the model issued them. kGraf's tools
write to a shared graph, so this matters more here than in most agent loops.

Cancellation is probed mid-stream, not only between turns, and preserves
tool call and result pairing. **Built:** steering a running worker. A note is
persisted to the session's inbox before it is acknowledged, then injected at
the next tool-batch boundary; an interrupt mode abandons the round in flight
first and closes its tool calls with synthetic results. A note to a finished
worker is refused and offered to the orchestrator.

Worker context is two parts (decision 0043). A **shared prefix** (system
prompt, tool definitions, and a pool of broadly useful graph slices) is
byte-identical across every worker on the same vendor and model, so the
fleet pays one prompt-cache write between them rather than one each. A
**private remainder** carries the brief, this worker's own slices, and its
transcript. Provider caches key on an exact prefix rather than on a
conversation, which is what makes the sharing possible; one worker-specific
byte before the breakpoint removes the effect entirely. Selective reading
beyond the pool happens the way it already does: the worker asks the graph,
and that read is charged to it alone. The Anthropic adapter also marks the
last block of the last message as a cache breakpoint, so the growing history is
cached and not only the system prompt.

**At its stop point a worker hands back, or compacts (decision 0095).**
Old tool outputs are masked on every request (decision 0057). When the context
reaches the stop point the worker hands its remainder back, and the
orchestrator makes it a child contract through the delegation guards. Where
the guards would refuse that child (the depth cap, the task length bound), or
the child could not start under the plan target, the runtime, never the model,
compacts the worker instead:

- one accounted summary call on the worker's own route, in a fixed JSON schema
  (goal, decisions, files touched, open items, constraints quoted word for
  word from the instruction); kGraf's own state fills what the summary lacks:
  the goal from the instruction's first line when the summary has none, the
  writes that succeeded when it lists no decisions, the changed files, and each
  declared write path not yet changed as an open item; an unusable reply gives
  a summary of that state alone;
- a `Compacted` event built by the loop alone, carrying a fixed notice (when,
  tokens, window, summary call, transcript path, items done and remaining); it
  reaches the worker's result, the trace and the run record, and counts as a
  sizing miss;
- a fresh private remainder: the instruction and slices, verbatim, plus the
  notice and summary; the shared prefix is unchanged;
- the earlier events stay in the append-only transcript, and the
  `recall_history` tool searches and reads them (the worker's own history
  only, bounded output).

At a later stop point the worker compacts again only when all of these hold,
checked in this order: the context right after its last compaction was under
the stop point (else no compaction can make room, and the contract can never
fit at this stop point); the rounds since that compaction made measured
progress (the progress gate's measure of decision 0096: net file change or an
improving check, and no sloppy pattern, against the tree and check scores
stored with the compaction, so a resume measures the same segment); and it has
compacted fewer times than the progress checkpoint ceiling (a ceiling of 1
keeps a single compaction). Otherwise it hands back with the reason (no room,
no progress, or the ceiling), which the trace records and the held reason
states, with the worktree and the transcript named. A later summary keeps
every earlier decision and quoted constraint word for word, so no summary is a
restatement of a restatement. Each compaction is traced and recorded; the
worker counts as one sizing miss. `/compact <worker>` compacts a running
worker at its next boundary, and only before its first compaction.

What describes the replaced context is cleared at the compaction: the stall
counts, since the earlier results are gone and a re-read is legitimate (a call
repeated three times after the compaction still stalls, and a resume resets at
the same round), and the one-continuation flags of an empty turn and an
output-ceiling stop. What describes the work carries across: the writes and
changed paths (so the no-change guard below holds for a compacted worker that
has written nothing, and not for one that wrote before its compaction), the one
no-change notice per attempt, the patch-rejection count, the step budget and
the progress checkpoints.

Two cases do not hand back. A worker that reaches the stop point with nothing written would give
its child the same whole task, so the runtime compacts it once instead (any policy), and after
that it goes on while its measured context is under the model's window. A hand-back is decided
only from measured context (the last reported usage plus the estimate of what was added since,
and the queued steering messages; reasoning output counts only where the route resends it, which
no adapter does, so a thinking model's reasoning is not in the measure of the next prompt); `Handoff::cause` and the trace's `compaction` record say
which (`measured_context` or `queued_messages`). There is no projection and no model-requested
hand-back.

**A hand-back's child is sized to fit.** The runtime words a remainder from the parent's whole
assembled brief, which for a child quotes its parent's brief, so each level wrapped the one
before and a child's first request grew with the depth. `handback_fit` brings the remainder back
to the chain's original instruction, once, with a marker line for the hand-back's level (the
guards' cycle rule needs the tasks of a chain to differ). The child is sized as a planned contract
is (0057): the route's prefix estimate and the instruction's bound must leave room under the plan
target (half the stop point), and the parent's pinned slices are cut, in order, to what is left,
each counted as the route's slice source renders it. The cap is stored on the child
(`slice_cap_tokens`) and applied at every dispatch, so a re-resolved slice set cannot undo it.
A child whose fixed part alone passes the target cannot fit: `stop_point_policy` spawns its
parent to compact (0095), and `resplit` refuses the split at the first hand-back with the numbers.

**A turn that only announces work is not a completion.** When an attempt would end as completed
with no successful write and no mutating command in the whole attempt (reads do not count) and
its contract declares writes, the runtime drops the turn, counts the round, sends one notice
(`NoticeKind::NoChange`) and continues; a second such end fails the attempt with the contract's
write paths named. A compaction does not lift this guard (0095).

**A retry carries what stopped the last attempt.** The stop note (`OpenContract::last_stop`)
holds the repeated call of a stall, a spent budget or a named failure, with the calls a policy
refused and no later call got past. The retry brief quotes it as data in the same tag as a
verifier refusal.

**A permission event states the final decision.** A mode's grant is staged at the hook that
makes it and recorded when the approval chain settles the call (`PermissionControl::settle_call`):
`granted` when the call runs, one `grant_refused` event (subject, mode, refusing hook, reason)
when a later hook refused it.

**Tools come from three places:**

- kGraf's own graph and file tools.
- **MCP servers.**
  - Transports: stdio for local servers; Streamable HTTP with OAuth for
    hosted ones (decision 0051).
  - Tool names are qualified by server.
  - A changed tool list applies at the next dispatch, never mid-run, because
    it sits in the shared prefix.
- **Skills** in the `SKILL.md` format, ingested from the user's other agents
  (decision 0053).
  - Only the catalog of names and descriptions sits in the prefix; bodies
    load on demand.
  - A skill's declared allowed tools are enforced by the runtime, never by
    asking the model: a call outside the list is refused on the worker's
    approval chain. The tool definitions stay in the shared prefix
    unfiltered. A subagent inherits its parent's restrictions. When a
    permission hook would ask about or deny a call a skill also refuses, the
    skill's refusal is reported, naming the hook's verdict, and nobody is
    asked. `report_shortfall` is never restricted, and a permission report a
    worker's skill refuses holds its contract with the skill's name (decision
    0053 amendment). A skill whose `allowed-tools` lists a tool the worker does
    not have is refused at activation, naming the entry and the real tools
    (0053, amended 2026-10-02).

A pre-tool hook can allow, ask or deny a call. The loop always holds its last
round back for an answer. Every call's trace records what it was given as
slice ids plus the prefix hash. **Not built yet:** deferring rarely used
tools behind a search tool, and an optional intent argument on each tool call
that labels the action on the graph at no extra model call.

**Trace and transcript fields a test can read.** The `dispatch` trace event has `brief_digest`
(the blake3 hex digest of the instruction the first attempt's worker opens with, before its
slices) and `brief_digests` (the same digest per attempt, keyed by worker id), so a test can tell
whether a retry carried new text: equal digests mean equal briefs. The `worker_finished` event has
`last_stop` (the normalized stop reason of the attempt's last model round that ended on its own:
`end_turn`, `tool_use`, `max_tokens`, `stop_sequence`, `pause_turn` or `other`; absent when no
round ended) and `tool_calls` (tool calls the attempt started, this run segment). A worker
transcript `stop` event is `{"type":"stop","reason":"end_turn","finish_reason":"stop"}`, where
`finish_reason` is the provider's own finish string as its wire carried it (`Response::finish_reason`,
set by the Chat Completions, Responses, Anthropic, Bedrock and Gemini adapters) and is absent where
the adapter reads none. Every field is serde-defaulted, so an older trace or transcript still loads.

The `plan_accepted` trace event is written once when a plan is accepted for dispatch, after intake
normalization and any re-asks, and again for each re-plan under the next `revision` (1 for the
first plan). It carries `contracts` (`id`, `instruction` in full as it is dispatched, `acceptance`,
`writes` as paths, `depends_on`, `route` when the plan pinned one, `task_kind` when set),
`interfaces` (`id`, `parties`, `symbols`; empty when the plan has none) and `notices` raised while
the plan was taken in (repairs, the unit-struct shape notice, "contract checks did not run for ...",
re-ask reasons, a merge of contracts that share one file and one test). Trace lines have no size cap, so the instruction is not shortened. A check can read the
plan from this event without a recording of the planner's answer. The `dispatch` event also has
`base_commit` (the integration branch tip the attempts' worktrees start from, or the commit a
remediation names) and `base_commits` (the same, keyed by worker id); both are absent when the
workspace provider has no integration branch. A resume that interrupts planning can write one
revision twice with the same content. Old traces without these fields still load.

**Built:** `ask_user_question`. The orchestrator first tries to answer from
the run record in one reserved, accounted call; the user is asked only when
it cannot. A question waits for the user a bounded amount of time (300 s in
a run started from the orchestrator box, 60 s otherwise), then returns a
recorded no-answer result that tells the worker to state its assumption
instead. A run the host declares unattended answers at once with that result
instead of waiting. The orchestrator's answer call waits a time that fits the
answering model (decision 0067, amended 2026-10-06): the user's
`answer_wait_secs`, else twice the 95th percentile of that model's last 50
answer and judgment calls in the ledger (its other generative calls when it
has fewer than 8), held between 120 s and 900 s, else 300 s. When the wait
passes, the worker is released as if the orchestrator had no answer, and the
call runs on (bounded at 900 s): its late answer answers the question if it
still waits in the box, is otherwise injected at the worker's next round
boundary, and is recorded only for a worker that ended. A connection
judgment fails at the same derived wait.

A worker receives a narrow brief and returns a compact structured result, so
the orchestrator does not receive the worker's intermediate tokens. The
effect this has on the orchestrator's own context has not been measured
inside kGraf itself; it is inferred from outside published figures.

`kgraf-runtime` holds the loop; `kgraf-tools` holds file edit and shell tools
(vendored from codex-rs, with `cap-std` confinement); `kgraf-mcp` holds
stdio and Streamable HTTP clients (on `rmcp`, OAuth only for servers
configured with it, header values only from environment variables, replies
scrubbed and bounded); `kgraf-session` holds the resumable transcript,
observation masking, and persisted allow rules. An unattended worker is
denied an MCP tool until an explicit allow rule is recorded. **Not built
yet:** the deferred tool-search tool and the per-call intent argument.

**Loop behavior worth knowing.** A successful write starts a new stall count, so edit and test cycles, and re-reads after a change, do not end a run as stalled. Observation masking keeps the last N rounds of tool results, every block of a round together. A resume replays only calls that succeeded. A paused turn fails loud. A stop at the output ceiling gets one counted continuation round, then fails. A turn with no tool call and no visible answer (a plan only in the model's reasoning) gets one counted notice round, and a second one in a row fails the attempt. A server failure on the model's own malformed tool call (HTTP 500 with a parse-failure body, as Ollama sends) is sent again within the attempt, at most twice in a row, each try its own ledger call and the retry carrying one added notice at the end of the transcript; after that the attempt fails and names the parse failure. `apply_patch` repairs what the model's intent makes certain, and each repair is a note in the result: a patch that starts with a file header and lacks `*** Begin Patch` is applied as if it had one; a missing or mangled `*** End Patch` is supplied (check that no lines were cut off); a Delete File and an Add File of one path are one replace; an `@@ -a,b +c,d @@` unified-diff header is stripped, and the text after it is the context line. Context and `-` lines match exactly first, then ignoring trailing whitespace, then indentation, then typographic punctuation; a looser match must fit in one place (several is refused, naming the lines) and is noted. A looser match is written in the file's own whitespace: each unchanged line stays as the file has it, and the `+` lines move by the one indentation shift the matched lines show (the note says how much); matched lines shifted by more than one amount refuse the patch, because the `+` lines' indentation cannot be known, and a Python or YAML file that parsed before and would not after is refused before it is written (decision 0084). An Update File chunk of only `+` lines names no place and is refused unless the file is empty or the chunk ends with `*** End of File`, because it used to append silently. A patch that names one path in two hunks (other than that replace) is refused, and its mismatch hints name the nearest match. A shell command's output is bounded as it is read, so a chatty command cannot grow host memory, and the command's process group is killed after a normal exit so background children do not hold the worktree.

**The round budget is a progress checkpoint (decision 0096).** A brief's round budget is one block. Where the loop would warn that three rounds remain, it measures the block instead (`kgraf-runtime/src/progress.rs`): files whose content changed on net (blake3 against the last checkpoint, the content captured before the block's first write, or git's dirty list), check runs whose pass, fail or error counts improved (cargo test and nextest, pytest, unittest, `node --test`, jest, vitest, cargo and `tsc` errors, else the exit status), and sloppy patterns (one identical error 3 times, failed writes outnumbering successes, write-then-revert, an identical call and result 3 times; a failed write whose every path a later successful write of the block wrote is made good: it counts neither as a failed write nor toward a repeated error). Model text is never read. The first block is measured harder: its changed files count only when they differ from their content at the block's midpoint (captured by the worker), and when the block ran a test or build command, none of those improved and all ended failing, changed files alone do not count (the checkpoint lists them as `files_discounted`). A progressing block gives the same worker another block in the same conversation; a block without progress after a productive one gets one grace block and a notice; a second idle checkpoint ends the attempt `BudgetExhausted` with no answer round; a worker that never progressed meets the old wall (the warning, then the final answer round), and so does one at the ceiling (`LoopSettings::max_checkpoints`, 10). Each checkpoint is a `WorkerEvent::Checkpoint` the resume reads back, and the result's `round_blocks`. The context check runs first at every boundary, so a full context still hands back or compacts. The orchestrator counts an extended worker as one sizing miss, traces `worker_checkpoints`, and counts `round_extensions` and `checkpoint_stops`.

**Transcript secret scrubbing (decision 0071).** Every event that can carry a
tool's or a model's free-form text is scrubbed on two independent paths.
**Model-facing** scrubbing redacts only kGraf's own exact-secret registry,
never a pattern rule, so a pattern rule's occasional false positive can never
corrupt code a worker reasons over or writes back to disk. **Persist and
display** scrubbing (feeding the persisted transcript, the shell, run traces
and agent messages) adds gitleaks-derived pattern rules, with a generic
credential rule additionally gated by entropy and gitleaks' own allowlist. A
persist-path marker is self-describing, since resuming a run rebuilds
context from the persisted, pattern-scrubbed transcript rather than from what
the live model actually saw. kGraf's own configured credentials register into the shared registry that backs both paths. A secrets file a worker reads (`.env`, `.env.*`, `*.env`, `secrets.json` and the like, at any depth, through `read_file`, `search_files` or a shell command that names it) adds its credential values to a records-only registry that only the persist path reads: transcripts, traces and run records lose them, and the worker still reads the file (decision 0071). A transcript file holds a
round's settled message, never its streamed deltas, so a secret split across
two deltas is never written; the model is shown an in-memory registry-only
copy of intake text; redactions are recorded as kinds and counts; deleted run
records are zeroed rather than merely unlinked.

### Orchestrator

The single director (decision 0004). Decomposes an instruction, or an
approved plan read from the corpus, into contracts; routes each to a
provider and model; spawns and retires workers; and answers the user. On the
corpus path it builds only from a plan the user approved by its plan id.
**Not built yet:** a gate that refuses to build without an agreed roadmap on
every path; an instruction given directly dispatches without one.

**Delegation is asynchronous:** issue the brief, take a handle, keep working.
**Not built yet:** a parent run id recorded per delegation. Worker reports
follow a five-kind delivery policy so a live graph plus chatty workers does
not overwhelm the user. The fifth kind, contract-inadequate (decision 0027),
is the only one that fans out to peers.

**The orchestrator holds no transcript.** Its state is the run record, which
compaction never rewrites (decision 0057). Each orchestrator call is built
from that record and re-grounds on the verbatim original instruction, so
there is nothing to compact. `/compact <worker>` asks that running worker to
compact at its next boundary, once (decision 0095). Worker transcripts stay
append-only, because editing an earlier message invalidates every thinking
block after it; a compaction appends its notice and leaves every earlier event
in place.

**Dispatch order is readiness, then leases, then run.** Readiness means
credentials present, MCP servers reachable, and tool discovery complete. A
worker that cannot run must never hold files another worker is waiting on.

**A route that fails at the transport is left behind, named, and probed
again** (decision 0075, `route_health.rs`). A worker call that could not
connect or whose connection broke takes its route out of rotation for a
cooldown that doubles with each fault in a row; fresh picks pass it over while
another route can take the contract, the attempt does not count against its
contract, and the trace records `route_fault`, `rerouted` and
`route_recovered`. With no other route the contract waits for the probe and is
held, naming the route, after three faults in a row. The run report lists
every route that faulted.

- **Spend is recorded, never limited** (decision 0080). Admission holds an estimate from
  dispatch to settlement so the ledger can compare it with the actual cost; no cap refuses a
  dispatch. A hard stop is set in the provider's console.
- **User text sent to a running orchestrator** is persisted in the run
  record before it is acknowledged, and the next orchestrator call reads it
  as data. It is never dropped.
- **Its routing and recursion-floor decisions stay frontier calls or fixed
  rules.** No local decider model ships (decisions 0042 to 0047): the
  measured problem is answered by a one-feature rule as well as any model
  tested.

`kgraf-orchestrator` holds decomposition into contracts, readiness,
admission with combined leases and budget reservation, delegation guards,
the report policy, verifier selection among attempts, the run-state record
with re-grounding, quality gates as work nodes, and the channel-independent
message port. `kgraf-host` builds a run for both the command line and the
desktop shell: routes behind their rate limiters, the ledger, worktrees,
merge-back and the gate runner. A run's record is one row in the graph
store; its worker transcripts and its trace are files under a per-run
directory. Verified work merges onto the run's own branch; the user's
checked-out branch never moves. A killed run resumes from its record with no
repeated paid call: a worker's request leaves only once its earlier rounds are
in its transcript, so a kill repeats at most the request in flight (decision
0056, amended).

**A malformed structured answer is asked again within one bound (0039).** Every orchestrator
call that asks a model for JSON handles a malformed answer the same way: an answer that is not
the complete object asked for (cut off mid-string with a normal stop, no object, the wrong
shape), and a server that could not parse the model's output, are asked again at most twice
(`MAX_MALFORMED_OUTPUT_RETRIES` in `kgraf-provider`, the bound the worker loop uses). The
single-shot calls (re-grounding, the question answer, the connection judgment, the handshake,
the consumer test, the challenge review, the interface review, question routing and the corpus
interpretation) go through `ModelCaller::ask_json` or, for the interpretation, a loop of the
same bound; a refused answer is asked again with the refusal quoted after the request, and a
server parse failure gets the same request again. The planner's contracts call and interfaces
call keep their own re-ask budgets, and a server parse failure is re-askable in both. Each try
is its own ledger call, settled against the call's hold, and a retry that then parses is a run
notice. After the bound the call fails and its error says how many times it was made. An
answer cut at the output limit is not asked again. An `error` object a server sends after it
answered 200 (in the body or mid-stream) is a typed error, never answer text, and one that
names a parse failure of the model's output is the same malformed-output kind as the 5xx.

**Contract checks and challenges (decisions 0029 to 0037)** are available
on the live path, off by default, opt in per run. When enabled, a second
planner call writes an interface contract for each place one contract uses
another's code, and each is registered in the run trace before any party
dispatches.
The sandboxed runner checks it: stub-compile, vacuity against the stub and a
reference, then a handshake, one call per party on that party's own route
through the ledger. A worker challenges with a dedicated tool; one challenge
per lineage and interface is open at a time, each worker has a fixed budget
of challenges, and escalation to the user follows only a paid review.

A refused planner answer is named where it can be corrected: an answer that is not JSON is
quoted around the point the parser stopped, marked `[ERROR HERE]`, with what usually causes that
error (a string left unclosed, a key without quotes); a type error names the field, the value
found and what the field takes; every interface file entry that is not a file is named at once.
Every refusal of the contracts call, the last included, is a `plan_refused` trace event; every
refused ask of the interface call is an `interfaces_refused` event; a run that fails planning
prints both.

With contract checks on, planning is two calls, and the second is made only for what needs judgment
(decision 0030, amended). The contracts call returns the contracts only. When the answer has two
or more contracts (or is a split, whose interface may bind the plan's other contracts), kGraf first
derives, with no model call (`derive_interface.rs`), for every producer of a consumed edge: the stub
(its declared files that hold a scaffold body in code, not in a string or comment: `todo!()`,
`NotImplementedError`; when none does, every declared file that exists, so a producer that edits
existing code has its current code as the stub), the consumer files
(the files that use its names: graph edges, and the consumers' own existing files) and the compile
command (`cargo test --no-run -p ...` over the packages that hold them, or a Python import; any other
language gets none, and the edge's reason names the language, `no_command_reason`). It runs
stub-compile and its canaries, and a derived check that passes is registered as an interface of kind
compile-only (`InterfaceKind::CompileOnly`, id `derived-<producer>`; no reference, no test, no
handshake, no consumer test, no stub land: its gate is the compile half and costs no model call;
`ContractCounters::compile_only_registered` counts it). When the producer's attempt passes its own
verification, `check_delivery` lays its delivered files over the check base and runs the compile
command again: a failure the base without the delivery does not have fails the attempt; a base that
fails too, or a check that cannot run, is a notice with the pass (`DeliveryCheck::NotJudged`), never a
hold. Then `behavior_plan` decides per producer
whether the planner is asked for a behavior check: not when a repository test already exercises the
producer alone, and not when every test that reaches it (by crate, module or item name, and through
the contracts it depends on) also reaches other contracts whose code does not exist yet, so that no
reference of the producer alone can pass it, and not when every file the producer declares is a
binary build output (a wheel, an archive, a compiled library: `binary_outputs_only`), which a text
reference cannot hold. Those edges are recorded as unchecked with a one-line
reason; a refused interface whose producer is one of them does not buy a re-ask; when no producer needs a check the interface call makes no planner call. Where one is asked
(its own budget of three asks), the request names what kGraf supplies per producer and asks for
`reference`, `tests` and `test` only, within 4,096 output tokens; kGraf fills the stub (declared in
`replaces`), the consumer files, the names and the compile command.
Each interface of an answer is taken on its own: parsed, its parties and shape
checked (symbols, stub, a reference covering every stub path, at least one test, both commands, no
path in both stub and consumer, every stub path a declared write of a party), the consumer and tests
files named by path alone read from the dispatch base, a stub file the repository already has listed
in `replaces` (0081), then its mechanical checks run (stub-compile with canaries, vacuity); a compile
command the sandbox cannot run, or that fails the stub-compile stage, is replaced by the derived one
and the checks run again, with no re-ask. A Python test file the test command runs as a script that
only defines its tests gets their calls appended under `if __name__ == "__main__":` before the
checks (`script_tests`), or, when a test takes arguments, is refused with that cause. An accepted interface is kept across asks; a re-ask covers
only the interfaces still refused or unbound, quoting those interfaces' previous entries and their
refusals (cut at 12,000 characters, the cut said). An omitted interface and a malformed one are
retried the same way. A compile or test command that names a script or source file the interface
does not supply and the repository does not hold at the dispatch base is refused with the file named
(`missing_command_files`), because the check runs before any contract does; a program that runs and
cannot open such a file is `CheckError::InputMissing`, not a missing program. Check output reaches no
model, trace or notice with the host's own toolchain noise in it (`filter_host_noise`: macOS `xcrun`
and `xcodebuild` lines, repeated identical warning blocks; error lines stay). At the end of planning
every consumed edge no behavior interface binds (a dependency, or a pair a refused interface declared)
is recorded as one `ContractEvent::Unchecked` per consumer, with the reason (why no behavior check was
asked or accepted, and the derived check's result), and counted one per edge in `checks_not_run` (the
report's `not run`) and in `unchecked_edges`; the book counts an edge once however often the event is
applied, so a replay and a resume agree. A notice says "contract checks did not run for ...". Each
interface ask is a `ContractEvent::InterfaceAsked` (calls, milliseconds, output tokens), so the
report states `interfaces  planner N calls S s T tokens; accepted C compile-only, B behavior`. The run
is not held for an interface the planner could not write.

When the gate holds a plan that nothing has acted on yet (no dispatch, no merge, no stub
landed, no interface the run was given), and the run would end on that hold, the whole plan
goes back to the planner once with the checks' failures, a handshake's included (decision
0031, amended). The re-plan's contracts call is re-asked like the first plan's on an answer
that is not JSON or breaks a rule, and its interfaces get their own budget; its ids are its own
(`re1.`), and a hold on it ends the run. A re-planned contract's brief carries, as a quoted data
block, only the held failures that concern it: those of interfaces that bound the contract it
replaces or name a file it writes.

The planner's answer is extracted leniently where the intent is certain: text through the last
`</think>` is skipped, the last top-level object that parses is the answer, and a raw control
character inside a string is escaped and the object parsed again. Contracts that declare one
identical write set which the verifier's narrowed check judges by one test file cannot be
verified apart: the plan is refused once naming them, and merged into the first contract if the
planner repeats it, with a notice.

A contract test may not replace a
file the project already tracks, and the stub, consumer and test files may
not name one path with different content. In the handshake, a provision
counts only from a party that writes the symbol's stub file; a shared touch
is an overlap only between parties no dependency orders; an expectation the
interface's own files state is not a disagreement with a silent provider; a
negated expectation is compared with a provision only when it names the
providing party or names no party, so an assumption about a third party is
not read as the provider's opposite (decision 0078, amended). A path is set aside as a claim unless it is a surface symbol: an interface over files with no named items (`pyproject.toml`, a module file) lists the files as its symbols, a sentence naming such a file is a claim about it, and a file is provided by the party that writes that stub file. A path anchor or a touch is one file however it is spelled (`./a`, `/a`, `a`), and the handshake prompt says `touch` lists only the paths the party will write (decision 0030, amended). An anchor is a CommonMark code span ("`` `x` ``" anchors `x`). A symbol is named bare, qualified (`a::b`, `a.b`, `A#b`), as a signature's or a call's name (`dotproduct(a, b)`, which claims no arity), or in an import line (`from a import b`, `use a::b;`, `import { b } from "a"`). A provision provides every surface symbol it lists with its subject ("`vectorops` (a Python package) and `dotproduct`") or that leads a later clause, each under the writer rule (decision 0078, amended). A sentence bound to no surface symbol is recorded as a warning and holds nothing. Each statement names its party's route and the model that stated it, each check event names every party's route and model, and the run record keeps every hold a check placed with the same parties (`check_holds`), so a hold is attributable to a model pair (0078 amendment).

The planner is told the project's names and not left to invent them (decision 0030, amended). Every decomposition prompt carries a section of at most 8 KB: the package names from the manifests (`Cargo.toml` package and workspace members, `package.json`, `pyproject.toml`; the first 3 KB), then the signatures, from the code graph, of the files and symbols the instruction names (a trait, struct, enum or class as its declaration, at most 1.5 KB each). What does not fit is cut with a count and a notice. A named file the graph holds no signature for is included as text up to the cap, with a notice. A template slot such as `<crate_name>` in an interface file or command is refused at intake with the slot named, and the planner is asked again. Two checks read the repository checkout (`plan_repo.rs`; not applied to a split's children): a file the instruction names that holds placeholder bodies (`todo!()`, `unimplemented!()`, `raise NotImplementedError`) and that no contract writes is refused with the file named and the fix (add it to a contract's writes); and a contract whose acceptance runs any cargo command in a workspace (`--workspace`, `-p`, `--manifest-path`, a `cd` into a member, a bare command, `cargo fmt`: cargo loads every member manifest first, measured with cargo 1.95) is refused when a member's manifest, or a Rust source for a member manifest the plan writes, neither exists nor is written by that contract or one it depends on, or when a command at the workspace root runs before the workspace manifest is written; the remedy names only shapes measured to build (decision 0030, amended 2026-10-06). The member list the root contract's instruction gives is read from a `members = [...]` list or, failing that, from prose after the word `member` or `members` up to the end of its sentence, where a word counts only when the plan writes or the repository holds `<word>/Cargo.toml` (decision 0030, amended 2026-10-06, second). A contract that writes a new package manifest under a workspace that, as it gets it, does not list the package, and that does not write the workspace manifest itself, is refused too: cargo then fails `-p` of the package and a command in its directory, and `--workspace` passes without building it (measured with cargo 1.99) (decision 0030, amended). A contract that writes a new package manifest under a workspace that, as it gets it, does not list the package, and that does not write the workspace manifest itself, is refused too: cargo then fails `-p` of the package and a command in its directory, and `--workspace` passes without building it (measured with cargo 1.99) (decision 0030, amended 2026-10-08). A contracts answer in which no contract declares a write is refused and asked again, the contracts named: a worker may change only the files its contract lists, so such a plan can change no file, and every check keyed on declared writes would pass it unjudged. One contract with no writes next to contracts that write is accepted (one that only installs and verifies changes no file); a write it attempts is refused at run time with a reason that names the plan shape (decision 0030, amended 2026-10-09, second). A worker that asks for such a file, written only by a contract that runs later, is held as a plan-shape error that asks for a re-plan. Every check that looks for a placeholder body reads one rule (`placeholder.rs`): the file is lexed with the tree-sitter grammar of its language, every comment and every string, raw string, byte string, character, template and regex literal is removed, and only the code left counts: a `todo!`/`unimplemented!` call, `raise NotImplementedError`, or a "not implemented" message given to `raise`, `throw`, `panic` or `fail`. A file that only mentions `todo!()` in a string or a doc comment holds no placeholder. A language with no grammar is read line by line, skipping comment lines; a prose file (Markdown, text) holds none. The template-slot check judges an interface file's code the same way. A re-plan quotes the held plan's failures (stage, command, stderr tail), and the re-planned contracts' briefs carry them as a quoted data block. The consumer canary breaks only consumer files that no tests file shares a path with; a shared file is broken by the test canary.

### Message bus

Carries briefs, results, and direct worker-to-worker messages. Records
everything, including messages that never touched the orchestrator, because
selecting an agent must show every message it sent or received.

Delegation is guarded: roster membership, a trace that must end with the
sender, cycle rejection, depth and size caps, and a policy table. Each pair
of agents resolves to one conversation file, named from the two ids in
canonical order (decision 0026), so a pair always maps to the same file and
the same graph node.

Each message is appended to a Markdown file under `docs/agent-comms/` in the
checkout the run works on, with its body fenced as data. That file is
committed to the integration branch and indexed, so a message is a node on
the change log. A verified contract's report goes to its dependents and to
contracts whose transcripts show they read a changed path, with no bound on
the number of recipients. Such a report reaches a recipient that is not
running in its next fresh brief, and is delivered when that attempt's first
model request carries the brief: an attempt that fails before one leaves it
undelivered. A recipient whose attempt runs, or resumes,
gets the same quoted data block injected at its next round boundary through
the runtime's injector, as a session inbox row, so the `Injected` event marks
it delivered; if the attempt ends first, the next fresh brief carries it, and a
report still undelivered when its recipient closes is traced `undelivered` and
reported to its sender (decision 0067). A worker's own outbound message goes
over a connection: open by rule (sibling, dependency, shared read, parent and
child) or by one batched, reserved orchestrator judgment, and delivered mid-run
by injection. Volume is never refused, only paced, and kept in order. The
sender's tool result says the message is queued, never delivered: only the
recipient's transcript shows delivery.

Every text another agent wrote reaches a worker as data, never as an
instruction: a direct message, a subagent's report, and a mailbox report are
each injected inside one escaped `agent_message` block that names the sender,
after a line saying it is information, not instructions. The user's own note
and the orchestrator's notices pass as they are. A subagent's report is traced
`report_queued` when it is handed to the parent's injector and
`report_delivered`, with the transcript sequence, when the runtime's `Injected`
mark is on its inbox row; a parent that ends before a boundary leaves it
`report_held` (the parent resumes) or `report_undelivered`, with a notice on
the parent's contract. After a restart, a report the parent's transcript
already holds is traced `report_delivered` once, from its inbox row's mark.
When a run ends, whatever its status, and also when it ends on an error, every
note still not delivered (its recipient held, never started, or stopped with
the run) is traced `undelivered` and reported to its sender with the reason,
once per note and recipient over every run of the record (decision 0067).

### Persistence and event spine

Three SQLite databases per project, all under a per-project state directory
(`.kgraf/`):

- **`graph.db`**: files, nodes, edges, layout, co-change, leases, work
  nodes, facts, schedules and their run history, run records, and the
  change log;
- **`ledger.db`**: the call ledger and its hash chain;
- **`schedule.db`**: the claim table for scheduled runs.

**Only facts are stored; display status is always derived at read time.** In
`graph.db`, triggers on the files, nodes, edges, layout, lease and work
tables append to the change log, and one poller fans out to the interface,
so no write to those tables escapes the interface. Worker transcripts and
run traces are files under a per-run directory, and agent messages are
Markdown files under `docs/agent-comms/`.

Every entry point opens the state directory through one function, which adds
it to the repository's local exclude file rather than editing the tracked
`.gitignore`. A repository that already tracks files there gets a warning
naming the untracking command, and an unreadable exclude file fails loud.
The same function migrates a state directory left under an earlier version
of the product, before it moved to `.kgraf/`: it repairs the git worktree
registrations and attempt refs and the absolute paths in run records and
configuration, moves the ledger unchanged, and refuses a project that has
both directories at once. Every refusal (git unusable, a live older process,
a symbolic link where it writes) comes before the rename, and a lock beside
the state directory lets one of many concurrent first uses win the move.
Merge-back refuses an attempt that touches either state directory.

**Not built yet:** a runs table with a parent run id, which would make
delegation a walkable tree and give the graph its own coordination edges.

### Instructions and skills

`kgraf-instructions` (decision 0053) reads, into one inventory:

- the instruction files the user's other coding agents already use (for
  example instruction files named for other agents), from user and project
  scope;
- their `SKILL.md` skills;
- any folder the user imports.

How the inventory is assembled:

- **Instruction layers** keep their order and provenance, and their imports
  are followed.
- **Byte-identical skills merge.** Two different skills with one name are a
  conflict the user resolves; nothing is chosen silently.
- **Every malformed file is reported.**

### Scheduled runs

Decision 0052. An orchestrator run can start on a recurrence rule or at an
instant.

- **Where it lives.** A schedule is a property of a work node, and each run
  is history on that node.
- **Firing.** Due occurrences are claimed atomically, and readiness is
  checked before capacity.
- **Failures.** A missing credential disables the schedule and says why. An
  unknown timezone is refused at creation.
- **The window can be closed.** The app keeps a visible tray process so
  schedules still fire.
- **Runs counted.** `schedule list` counts clock-started occurrences and
  manual `run-now` runs together; a manual run with no outcome yet is counted
  apart.

`kgraf-schedule` parses recurrence rules and owns a fenced SQLite claim
table. The scheduler loop lives in `kgraf-orchestrator`; the desktop shell
runs it on a background thread and builds each due run through `kgraf-host`.
The tray shows one aggregate line. Each claim, start, deferral, outcome and
disable reason is recorded on the work node; an attached schedule can be
created against an existing node or a new one from an instruction.

### Desktop shell and event bus

**One native Rust binary** (decision 0054): `winit` for the window and
taskbar icon, `tray-icon`, egui for panels, and `wgpu` for the galaxy.

The design constraints behind it:

- the lightest program possible;
- a dedicated icon;
- a fully local harness;
- **no guaranteed GPU.**

A webview's WebGL falls back to software without telling the app. `wgpu`
names its adapter and can take a software one, so CPU rendering is a stated,
detectable mode rather than a silent degradation.

Typed events flow from core to UI (graph mutations, agent lifecycle,
messages, tool activity, token spend), and commands flow back (user
utterances, selection, play, stop and order). A note or redirect to one
session is persisted, then injected. Both sides are Rust, so the contract is
plain Rust types and there is no cross-language boundary to drift.

`kgraf-shell` (binary `kgraf-app`) holds the window, panels, tray, adapter
selection with the adapter type shown, a probe/force-software mode, and a
settings file holding the compaction slider and the background-mode switch.
It is packaged for macOS, Linux and Windows.

Its modules:

- `theme.rs`: the visual token set for both light and dark themes, and five
  semantic colors read under the chosen color-vision mode; embedded fonts
  and an icon font.
- **Color vision modes (decision 0070).** Beyond the standard theme, four
  color-blind modes (protanopia, deuteranopia, tritanopia, achromatopsia)
  each carry their own table of the five semantic colors per surface, chosen
  so the colors stay distinct under a simulation of that deficiency while
  keeping WCAG AA contrast. The setting picks the mode and it takes effect on
  the next frame, no restart required. The galaxy renderer will pick this up
  once its own rendering reads the same semantic-color source.
- `layout.rs` and `chrome.rs`: the masthead, the guide, the chips, the stage
  and its overlays, the cards, the settings dialog. The galaxy renderer is
  `kgraf-galaxy`'s, used through its public API; the shell draws only
  overlays over it.
- The settings dialog has five tabs (General, Runs, Permissions, Appearance, Providers),
  each drawn from `settings_rows.rs`: a section heading, a row's label and one-line
  description (CMP-SET-1), the divider between rows, a switch whose label is its
  accessible name, and a note (CMP-SET-6). `settings_dialog_tests.rs` drives each control
  through the accessibility tree and checks the effect it reaches.
- `projects.rs` (decision 0066): a workspace of open projects. Each has its
  own host, scheduler, graph view and runs; the frame polls every one, so a
  background run advances. Row indicators and the menu button's tint come
  from one priority function over live signals.
- `activity.rs` (decision 0063): a thread per shown project reads the
  change log's watermark and, when it moves, the newest rows for lease
  takes and releases, plan, schedule and presumed-node events, and files
  coming and going, filtered before the row limit; each row routes to its
  node through the galaxy's model.

### Work hierarchy and scheduling

Work is a recursive node hierarchy distinct from code nodes and agent nodes.
A part decomposes into sub-parts until one fits a single worker's brief.
Work nodes live in the same store as code nodes, with dependencies between
them as edges. **Not built yet:** edges from a work node to the code nodes it
is expected to create or modify, so the partition plan and the interface
read the same structure.

Doneness and blocking are **derived, never stored** (decision 0007).
Doneness aggregates over children, with a leaf's state coming from its
worker. Blocking is derived from an unfinished dependency, a denied write
scope, or a worker reporting stuck or needs-input, so a node becomes
visibly blocked the moment the cause exists.

The scheduler's scope is the **whole work graph**, not one item (decision
0016). A node is eligible when its dependencies are met and admissible when
the leases on its declared write set are free; both checks must pass, and
they are kept separate so the interface can distinguish a blocked node from
a colliding one. Concurrency is an explicit cap.

**Autonomy is bounded** (decision 0017). A user-facing cap limits concurrent
work items, and a separate ceiling limits total live agents, since recursive
decomposition means those are different numbers. Both are enforced
pre-flight by refusing a spawn, never by killing one after the cost is
incurred. A node held back by the cap is a third visible reason, alongside
blocked and colliding.

**Run limits are the user's global settings** (decision 0017, amended). Rounds per
worker, the worker reply cap, the orchestrator call cap and the assumed context window
are sliders in Settings and keys in the user-level settings file, never a project file a
worker could write. A command-line flag outranks the setting, which outranks the built-in
default (30 rounds orchestrated and 12 for a single worker, 16,384 and 32,768 output
tokens, a 128,000-token window). An output cap is lowered to the model's supported limit
(`kgraf_core::output_limit`, from the bundled catalog) with a notice.
The desktop also sends its compaction slider through the same shared value
(`UserLimits::compaction_point`, set only by the desktop): a run file's `point` outranks it, and
the command line keeps `--point`.
A sixth key, `warn_rule_conflicts` (off by default, a checkbox in Settings > Permissions), rides in
the same file, shared value and precedence but limits nothing: when on, a run's start checks
the user's verify and configured gate commands against the user's deny and ask rules
(`kgraf-host/src/rule_conflicts.rs`, using the approval chain's own matcher) and says, as a
`notice` line and a trace notice, which programs the rules block for workers. It never changes
the run.

Scheduling has two modes. A play control hands the orchestrator everything.
Alternatively the user selects nodes and an order, and the orchestrator
completes that partial order. **Admission control binds in both:** a
user-queued pair that collides runs in sequence, with the reason stated.

For an existing repository, doneness is reconciled against the code graph
rather than read from a documented status, so a plan claiming completed work
with nothing behind it surfaces as a discrepancy.

**The hierarchy is dynamic** (decision 0015). Children are created by
decomposition, complete, and are superseded during a run. Completed children
collapse into the parent's doneness. Code nodes fold into their parents and
expand on click (decision 0068, below); **not built yet:** the same for work
nodes, whose children are a run's contracts. Because decomposition is
ongoing, **partitioning is continuous**: a sub-node can appear mid-flight
colliding with a running worker, and every available response to that is a
form of serialization.

### Galaxy graph UI

**Layout is computed in the Rust core and cached in the database**, so the
renderer never runs a force simulation and instance matrices are built once.
The renderer is a `wgpu` instanced renderer that takes its shader techniques
from rerun's `re_renderer`, copied in so it can be reshaped freely. It has a
**CPU mode** sized for a software rasterizer:

- point sprites;
- edges on selection only;
- no animation when nothing changed.

The layout is anchored rather than free: deterministic directory-ring
seeding, depth on the third axis, and a per-cluster disc that grows with the
cluster's size in power-of-two buckets. There is no force refinement:
measured, it moved every node on any insert. Anchoring separates clusters by
construction and keeps positions stable across reindexes; a node moves only
when its cluster crosses a size bucket or its own call depth changes.

Code nodes are colored by stellar class and sized by degree. **Agents get
their own visual channel** and must not reuse either encoding. Selecting an
agent drops everything unrelated to a fraction of its color and keeps its
live edges near full intensity.

At scale, a washed-out view is caused by edges, not nodes, so edge intensity
dims on an inverse-square-root curve of the edge count. **Not built yet:** a
bloom pass, and boosting node glow by channel dominance rather than raw
brightness, because bloom is luminance-thresholded and blue carries almost
no luminance weight.

The galaxy is the only work surface; no separate board or list view exists
(decision 0003).

`kgraf-galaxy` holds the layout, an instanced `wgpu` renderer with a
CPU-mode node cap, and a live feed from the graph store's change log with
agents and work items as their own populations. About 1 ms per frame at
100,000 nodes on an Apple M4 Max, measured by its own benchmark suite. The
interaction layer sits on the same crate: the orchestrator's input box on
the message port, with a run command starting a run through `kgraf-host`;
approvals answered in place; play, stop and the user's order; search whose
result is a graph selection; one text surface; and status on work nodes,
including a cap hold distinct from blocked and colliding. Scheduled work
nodes carry their schedule, last run, run id, spend and any disable reason
on their label. **Not built yet:** expanding a work node into its children,
call traces on nodes, and questions from a worker reaching the input box.

**Folding (decision 0068).** A large graph folds children into their
parents, so it stays a readable picture. The hierarchy is the code's own: a
chunk under its containing parent, a file under its directory, a directory
under its parent directory, with single-child directory chains merged. Each
directory is a **proxy**, drawn at a position that is a pure function of its
key. No node has more than 64 children: a longer list, the root's included,
splits into group proxies over sorted ranges, so one expansion draws at most
64 more nodes. The automatic cut draws at most 1,000 hierarchy nodes,
opening parents in ascending depth-then-child-count order, and keeps its
previous cut while it still fits (hysteresis), so one change moves the view
by about the size of the change. Agents never fold. Work nodes fold too,
folded by default and never opened by the automatic cut: a presumed item
under its section, a handoff child under its split contract, a gate under
its major contract. A click on a folded node expands and selects it; a click
on the selected node folds it; expand and collapse handles do the same,
drawn only on the selected, the hovered, and large folded nodes. User choices
sit on top of the automatic cut, keyed by graph id, for the life of the
view; past the budget, and for refused cycles, the view says so even with
its status overlay hidden. Only items that need the user are counted, once,
on their work node, in one count indicator (failures included); a distinct
mark shows the failed item itself; a hidden node's count goes to its drawn
ancestor or its group. Every color comes from one shared palette, so the
shell's chrome and the galaxy's own drawing read the same source. The
layout, fan-out and click logic are pure functions, testable without a
window.

### Worktrees, merge and gates

Each writing worker gets an isolated git worktree, created locked and pruned
on startup, with lifecycle driven by the git CLI rather than a library
binding. **Worktrees merge back continuously** as each worker finishes
(decision 0018), so conflicts arrive one at a time and attributable.

**Quality gates are work nodes**, not background jobs. One attaches to
**every major node**, meaning every node with children, so gates sit at the
joins of the hierarchy rather than on a timer (decision 0019). They run the
validation commands discovered from the project's own documentation, against
a **pinned commit** so merging never freezes.

**A work node is done when its code exists and has passing tests.**
Producing those tests is part of a worker's brief, and doneness derives from
both, so code without tests derives as partial rather than complete. The
test runner is discovered from the corpus; the test content is produced by
workers, which is how "gates are never invented" and "agents write tests"
are both true. A failure halts new dispatch without canceling in-flight
work and produces a remediation node. Because a gate is itself a node, a
failing gate is already a blocking node with a visible edge to what it holds
up, needing no separate state model.

Discovery takes `cargo test --workspace`, `npm test`, `python3 -m pytest`,
`go test ./...`, `mvn -o test`, `./gradlew test --offline` or `gradle test --offline`,
`dotnet test` and `make test` from
the project's build files, each only when its tool is on `PATH` and the project's
dependencies are installed; a gate that could not pass on correct work is left out, and
the run says why and what to run (decision 0018, amended 2026-10-06).

The discovered commands cover the whole project (`cargo test --workspace`),
so a gate over part of the plan also runs tests of parts not built yet. When
a gate below the root fails, the same commands run once per run at the
commit the run started from. If they fail there too, the gate is
**inconclusive**: it opens no remediation, halts nothing and blocks nothing,
and the root gate, which runs once everything is built, judges its cover.
The root gate is never inconclusive. A remediation carries the failing
gate's commands, and its verifier runs them after the run's own check, so a
remediation is accepted only when the gate's commands pass in its worktree.
A remediation that integrates no change counts as failed: the gate opens the
next one or holds, and never runs again on the unchanged commit.

**A contract is accepted on its own tests.** When the acceptance command is a
whole-suite runner kGraf recognizes (`unittest discover`, `pytest`, `node
--test` or `npm test` over it, jest or vitest, `cargo test` over a
workspace), each attempt runs only the tests that belong to its declared
files by the runner's naming convention, and the brief names that command.
The whole command is added as a root gate when it is not one already, so it
judges the merged result. The acceptance command is a root gate in every case, narrowable or not (decision 0018). A whole command that fails on an attempt while other contracts of the plan are open, and also fails at the attempt's dispatch base, is deferred to that root gate for that contract: the attempt merges after its scope and placeholder checks, with a verdict and a notice that say so, and the run completes only when the root gate passes. A command the attempt broke (it passes at the dispatch base) is a failure; the last open contract is checked whole. A failing whole check of every committed attempt runs at the dispatch base, open contracts or not, so the verdict of the last contract, or of a run with no root gate, says whether an earlier merge of the run broke the check (it passes at the run's original base) or the check failed before the run's work (decision 0018). Anything else runs whole, as do gate remediations; the brief then still names the whole
command as what the attempt is judged by, calls the contract's Acceptance line advisory,
and says `todo!()` panics and failing tests in other contracts' unfinished code are not the
worker's to fix, while a compile error anywhere that the worker's change caused is its own.
An attempt that changed none of its declared files is refused whatever its
check says, unless an earlier merge of the run delivered those files and the check fails on
the run's original base: that attempt is accepted as an empty integration, with an
`unverified:` notice naming the earlier contract.

Gates are also what makes an imperfect partition survivable. The
interference the underlying research most warns about is **semantic**:
changes that merge cleanly and then fail each other's tests. Git cannot see
that class, and a static analysis of blast radius cannot measure it either.
A gate catches it directly, by running the tests.

A repository with no discoverable validation commands gets no gate, and the
orchestrator says so at ingestion rather than inventing a test command. A
worker declares the files and graph nodes it intends to write before writing
them, and the orchestrator admits or defers.

**Write leases (decisions 0048 to 0050).** One global registry, keyed by
path, is the database's own exclusion mechanism.

- **Deadlock cannot be constructed.** A whole write set is taken atomically,
  in sorted order, so a worker never holds part of what it needs while
  waiting for the rest.
- **An unleased write does not compile.** A lease type has no public
  constructor.
- **A blocked acquisition decides nothing.** It returns every conflict with
  its holder and expiry, and the orchestrator arbitrates.
- **File granularity over-serializes.** Under measurement, close to half of
  near-concurrent waits touched no genuinely overlapping region, so sub-file
  leases keyed on stable node ids exist too. File and node leases are
  admitted in one transaction, through a layered write-set scheduling design
  ported from Haystack.

**Execution environments are reached through one workspace interface.** One
runtime is built: git worktrees. **Not built yet:** other execution runtimes,
each capped by its own capability limits, and sealing a task's environment
choice at first dispatch.

**The verifier judges write scope before the check runs.** It commits the
worker's change first, judges scope on that commit, and merges only it. The
check then runs in the same worktree. When the check's shell exits, its
process group is killed before the leader is reaped. What the check leaves
behind is removed by a walk that opens each folder without following
symbolic links, relative to its parent's own descriptor, so a background
process that swaps a folder for a symbolic link cannot steer the removal
outside the worktree; on Windows that removal refuses rather than risk it. A
lockfile the worker generated, cache directories, and a Python package
build's output beside its project manifest (`dist/*.whl`, `dist/*.tar.gz`,
`*.egg-info/`, `build/lib*/`, `build/bdist.*/`; decision 0084, amended), and
any other language's install, build or test output beside its project
manifest (`node_modules/`, Maven's `target/`, Gradle's `build/`, `bin/` and
`obj/` of a .NET project, `.venv/`, ...), tool caches, `.class`, `.o` and
`.tsbuildinfo` files, and a compiled program told by its first bytes, are
left out of the commit, with a notice, and removed before the check, so the
check runs against the committed tree only; the scope check does not count
them as writes. A check that changes committed files fails
verification.

A descendant process that calls its own session-detach call escapes the
group kill. On macOS and Linux, a gate command runs under a CPU time limit
at twice the gate's timeout, and a contract check at a fixed CPU-second
budget per process by default, so such a process cannot spin without limit.

Every file lock is released with an explicit unlock rather than only a
close, because a lock taken with `flock` belongs to the open file
description, and a child forked by another thread keeps a copy of the
parent's descriptors until it execs; a close alone releases nothing in that
case. Two defects this discipline fixed: a child forked while the parent
held an exclusive lock could inherit it and deadlock waiting on its own
copy; and the next check of a free lock could read a leftover lock as busy.

**On Linux, cargo is granted read access only to trusted files above a
worktree.** A worktree lies inside the checkout, and cargo reads each
ancestor's manifest and config files; the sandbox otherwise refuses those
reads entirely, which broke cargo commands whenever the crate under test was
not a workspace member, or the repository had its own cargo config. The fix
has four parts:

- **Opt-in and bounded.** Ancestor reads are granted only up to a checkout
  root the caller names. The gates, the verifier, and the shell tool opt in.
  The contract-check policy never does, because its tree is a copy under a
  scratch root, where another user could plant a config file that would
  forge a pass.
- **Owner and mode trust.** A file is granted only when it and every
  directory up to the limit are owned by this user or root and not writable
  by others. Group-write is trusted only for this user's own private group.
- **One open, no links.** Each file is opened once, a component at a time
  from the limit, with symbolic-link-refusing, boundary-checked path
  resolution, and the same descriptor is granted to the sandbox as a
  read-only rule, never execute. The user's own credential and cache
  directories, and any explicitly denied path, are never entered.
- **Fail loud.** Each existing cargo file that is refused is reported with
  the path at fault and why. The gate, the verifier and the shell tool
  append it to a failing command's output, and startup reports it once per
  run.

macOS reads are governed by a deny list and already allow these files;
Windows does not confine reads.

**Every new worktree is a from-zero build environment**, since build outputs
and dependency directories are untracked. Each worktree's target directory is
instead seeded by a clone (decision 0060), measured to cost less than a
from-zero build. Continuous merge-back is ported from container-use and runs
in the object store; each worktree builds into its own target directory.

Sandboxing uses native primitives per platform with no administrator
requirement. Strength differs materially: Linux gets Landlock with seccomp
and namespaces, macOS gets Seatbelt, and **Windows gets materially less, with
no non-admin way to block network egress at all.** Each backend reports its
own strength and gaps. Because Windows cannot deny the network, the
verifier's check, the gates and the contract checks run there with the network
open and a notice in their output, the run's trace, the run report and the
run-start list (decision 0011, amended 2026-10-06); macOS and Linux keep it
denied. The worker's shell runs on Windows under the same decision (amended
2026-10-07): the network open, the backend's partial isolation accepted, and
the notice in the tool's description, its first result, the run's trace and
the run-start list. On Windows `sh` and `bash` run with Git Bash, the argv
carried in an environment variable under a fixed trampoline script, because
the MSYS2 runtime splits a command line with its own quoting and globbing
rules; with no Git Bash the command fails and says how to get one. Compiled
for Windows, not yet run there; the test steps are in
`docs/WINDOWS-TESTING.md`. **Not built yet:** a per-platform strength indicator in
the interface rather than one shared badge.

`kgraf-sandbox` holds worktrees through the git CLI; macOS Seatbelt; on
Linux, user, mount, IPC, PID and net namespaces with Landlock and a seccomp
filter ported from codex-rs; on Windows, a restricted token, root ACLs, a job
object and a low-integrity token, compiled but not yet run live. Escape
tests pass on macOS and on Linux, arm64 and x86_64. Windows sandboxing is
compiled only.

### Diagnostics

Local-only usage counts and errors, never sent anywhere: this crate has no
network code at all. One capped, per-user JSONL file records launches, runs
started and finished, workers spawned (provider and model names, never a
credential), named feature use, and every error under a stable glossary
code. No secrets, file contents, prompts or completions ever reach it; a path
is written relative to the project root when it is inside it, or hashed with
a keyed BLAKE3 (a random key generated once per install and stored separately
from the diagnostics file) when it is not, so an account name in a home
directory path never appears, and the hash cannot be reversed by an offline
dictionary guess. Context is capped at 512 bytes per event.

**Format: JSONL, not a length-prefixed binary or a compressed stream.** The
user must be able to read the file directly, or through a `diag show`
command; a binary framing serves only automated reading, and a compressed
stream is not cheaply appendable at the size this file targets. Over the
cap, the writer thread trims the file to its newest three-quarters, snapped
to a line boundary, rather than keeping a second rotated file. Two processes
appending at once hold an advisory lock on a sibling lock file, never the
data file, across the whole append-and-maybe-trim, and write each line as
one atomic write rather than two, since both partial writes and lock
ordering were measured causes of corruption under concurrent kGraf
processes.

**Never on a hot path, and never lost at exit.** Recording an event is
non-blocking, onto a bounded channel to one background writer thread; a full
channel drops the event, counted and reported into the file itself on the
next successful write, rather than blocking the caller, and nothing here
ever panics. A binary holds a guard whose drop flushes the writer on every
return from its entry point, since a static's own drop does not reliably run
at process exit.

**Specific error codes.** A shared trait, implemented on the error types of
every major crate, lets both binaries' top-level error handling walk the
error chain and record the most specific code found; an unrecognized chain
falls back to a catch-all code.

A `diag show` command prints a human summary and warns if the file is not
writable; a `diag export` command writes a re-scrubbed, shareable copy the
user can send however they choose. Environment variables redirect or
disable diagnostics entirely, for a test harness that must never touch the
real per-user file.

**Not yet handled: non-path secrets.** A credentialed URL or an API key
embedded in a provider's HTTP error body reaches an error's context the same
as any other text; this crate's own scrubbing only looks for paths. A single,
clearly marked call site is a pass-through today, meant to route through the
transcript-secrets scrubber described above.

`kgraf-diag` is wired into the command line, the desktop shell, the host
crate and the orchestrator.

### Crate map

| Crate | Holds |
|---|---|
| `kgraf-core` | ids, the token-usage normal form, money, budgets |
| `kgraf-graph` | chunker, SQLite store, queries, slices, co-change, leases, temporal facts, work nodes, repo-map ranking |
| `kgraf-provider` | the provider seam, the ledger, the shared streaming decoder, rate limits |
| `kgraf-provider-anthropic`, `kgraf-provider-openai`, `kgraf-provider-bedrock`, `kgraf-provider-google` | one adapter crate per wire protocol |
| `kgraf-fleet` | the route catalog and credentials by environment variable |
| `kgraf-decide` | the typed-decision seam; no weights ship |
| `kgraf-instructions` | the instruction and skill inventory |
| `kgraf-runtime` | the worker loop |
| `kgraf-tools` | worker tools and command policy |
| `kgraf-secrets` | secret detection and redaction: gitleaks-derived pattern rules plus kGraf's own configured-credential registry (decision 0071) |
| `kgraf-mcp` (+ `kgraf-mcp-test-server`, tests only) | the MCP client |
| `kgraf-crosslimit-hammer` | a second real process against the shared rate-limit store, used only by the provider crate's crash and multi-process tests; never published or packaged |
| `kgraf-session` | transcript, approvals, skills at run time |
| `kgraf-capacity` | the per-machine parallelism maximum: route rate limits, memory, file-descriptor and writer bounds, measured and combined |
| `kgraf-orchestrator` | decomposition, contracts, checks, gates, run record, scheduler loop, templates, demand |
| `kgraf-host` | run construction shared by the command line and the shell |
| `kgraf-schedule` | recurrence and the claim table |
| `kgraf-ingest`, `kgraf-interpret` | corpus parsing, and the interpretation pass |
| `kgraf-sandbox` | worktrees, sandboxes, merge-back, per-worktree build directories |
| `kgraf-galaxy` | layout and renderer |
| `kgraf-shell` | the desktop app (`kgraf-app`) |
| `kgraf-cli` | the `kgraf` command |
| `kgraf-diag` | local-only usage counts and errors, and the error glossary |
| `kgraf-bench-briefs` | a per-model brief-following bench: fixed small briefs, mechanically checked (decision 0075) |
| `kgraf-tbench-bridge` | benchmark-only: routes a worker's shell commands into a Terminal-Bench task container; not part of the shipped app |
| `kgraf-soak` | dev-only soak driver: many runs on one `Host` in one process, one JSON line of process resources per run; never published or packaged |

## Data flow

**Spin-up.** `kgraf index` builds the code graph and reads the project's own
roadmap documents into presumed work nodes (decision 0064), so the galaxy
shows the plan the documents describe, reconciled with the code, before
anything is approved. No roadmap is a stated line, not a silent gap. The
desktop shell runs the same presumed-node read when it opens an
already-indexed project; opening a project never builds the code graph, so
a project `kgraf index` has not yet touched shows a "not indexed" notice. A
run does build it: every path that starts a run indexes the working tree
incrementally before anything is built, so workers and the planner get graph
slices on a fresh checkout. A graph that is still empty after that is
reported as a notice.

**Cold start, two paths.** If the project already has documentation in any
shape, `orchestrate --corpus` ingests the whole corpus, interprets it into a
plan, and prints what it understood the project and the plan to be, with a
plan id; a correction flag reads again with the user's correction. In an
empty directory the user gives an instruction too, and the reading is a
greenfield plan. On approval by plan id, the plan seeds the graph with work
nodes, and no worker runs on this path before that approval. A run started
with a direct instruction alone skips the reading.

**A unit of work.** The orchestrator selects the next item, writes a brief,
chooses a provider and model or honors a pin, and spawns a worker into its
own worktree. The worker requests context slices rather than reading whole
files. It takes leases on what it writes, and the change log turns those
leases into live edges from the agent to the nodes it holds. It returns a
compact structured result. Where the item was attempted more than once, a
verifier selects the winner, and verified work merges onto the run's branch.
After an accepted merge the contract's written paths are reindexed from the
run's integration view, one detached worktree at the integration tip that
the host moves to each new tip and keeps for the run, so the next contract's
slices see them. The winner's own worktree is not read, because under the
default worktree retention (decision 0088) the merge removes it. The indexer
refuses a source root that does not exist, so a vanished tree is a traced
failure, never every merged file deleted. A contract keeps its slice names,
and dispatch resolves them again against the graph as it is then: a node id
hashes the symbol's signature, so a predecessor that renames a parameter
gives the symbol a new id. A name that no longer resolves is a traced empty
lookup, not a failed spawn. When none of a contract's named slices resolve, or it names none, it is given the top-level symbols of the files it declares it writes (at most 8, about 4,000 tokens together), traced as a notice; the run report's `slices` line counts the dispatches that had slices, those that took the declared files' symbols, the named lookups that found nothing, and the slices left out of a brief. A worker's slices are read from its own worktree, at its dispatch base, never from the checkout (decision 0043, amended 2026-10-06): a slice whose file is not in that tree, holds another version there, or cannot be read, or whose node is gone, is left out, and the worker starts with a `slices left out` block in its opening and one trace notice per slice naming the node, its file and the reason; only the graph's database failing stops a spawn. A conversation file is indexed from a worktree removed once it is committed, so after the commit it is indexed again from the integration view, and after each integration every recorded source root that no longer holds its file is re-pointed at the view when the view holds the same content. A contract whose projected block is over the
plan target is split only when a child could fit: every child carries the
worker prefix and the projected transcript growth, so when those alone are
over the target the contract is dispatched unsplit, with a notice that gives
the numbers. **Not built yet:** reindexing triggered directly by a worker's
edits before its merge is accepted.

A worker builds and tests its own work, in any supported language, against
the dependencies the user installed. Each installed tree in the checkout
(`node_modules/`, Yarn's Plug'n'Play files, a `.venv/` with its `pyvenv.cfg`,
`vendor/`) is linked into the worktree, gate worktree or contract-check tree at
the same place, and the sandbox lets a command read and run programs from it,
never write it (`kgraf_sandbox::link_dependency_trees`,
`SandboxPolicy::with_dependency_links`); a workspace install's links to its own
packages resolve to the tree's copies, and the virtual environment of the folder a
command works in is activated; a pnpm install with workspace packages is cloned
copy-on-write instead, so its nested workspace links resolve in the tree too, and on
Windows the links are directory junctions. Every check runs with a temporary folder of
its own, so tool settings a worker left in its scratch folder never reach it. The
verifier's check also builds into a cargo target folder of its own, outside the worker's
writable roots and cloned from the checkout's `target/debug` when there is one, and the worker's
ignored files are removed from the tree first (`kgraf_sandbox::CheckFolder`,
`MergeBack::remove_ignored`): build records the worker left never decide a check. The package stores in the home folder (`~/.m2/repository`,
`~/.gradle/caches`, `~/.nuget/packages`, `~/go/pkg/mod`, `~/.npm`,
`~/.cargo/registry`, ...) are readable and their configuration and credential
files are not. A missing install of another package manager is named, with
its exact command, at run start and after a failing check or gate
(`kgraf_host::deps::installs`); with the user's consent for that command, once per
project, kGraf runs it at run start in its sandbox (`kgraf_host::deps::install`):
writes only in the checkout and the project's package stores under
`.kgraf/package-stores/` (`kgraf_sandbox::PackageStores`), `.kgraf` itself hidden
(`SandboxPolicy::hide`), `.git` read-only, no login file of the user's read, and on
macOS and Linux the network limited to the ecosystem's registry hosts through kGraf's
egress proxy (`kgraf_sandbox::egress`, `Network::Proxy`: Seatbelt allows only the
proxy's loopback port; on Linux the command's own network namespace hands its loopback
listener to the proxy). Windows runs it with the network and reads open and says so. The
consent is sealed with a per-user key, so a consent file a repository brings counts for
nothing; no process the install starts outlives it. Later
commands read the project's stores read-only (decisions 0084, 0087 and 0011, amended
2026-10-06 and 2026-10-07).

A worker builds and tests its own work. A declared allowlist of build and
test commands (cargo's, and each other language's test and build commands,
never an install or a publish) runs without approval in the sandbox with the
network off (decision 0084); under a write scope nothing else that can write runs, unless
the user's permission mode grants it. A command line sent as one string is
split into its words when it holds no shell syntax, and every check judges the
split argv; one with shell syntax is refused with the way to write it. Workers
are told kGraf commits their worktree after its check. A worker refused by
policy, or short of instructions, reports it with `report_shortfall` and stops (decision 0093);
the user's permission mode decides what happens (decision 0094). The mode is
one live setting that only the user sets, from the box's dropdown, the
user-level `permissions.toml` or the command line: Manual asks the user,
Accept edits grants file edits, Auto grants the user's allow set, Orchestrator
decides lets the orchestrator grant or refuse each request without asking, and
Bypass grants everything. A command that downloads packages (`pip install`, `npm ci`, `cargo fetch`) is the exception: outside a disposable remote environment it is refused in every mode and under every grant, with a reason that names the setting that allows the work, and a shortfall report of it holds the contract instead of granting the program (decision 0087, amended 2026-10-09). Where a mode or a contract's grant covers named
programs, an `sh -c` script of simple commands is granted when a grant covers
every command's program, as Claude Code allows a compound command; a script
with substitution, a subshell, backgrounding or a redirection outside the
worktree is asked about whole, and the question names the command no grant
covers. A script's command is also covered by the build and test allowlist's
cargo rules, and once per script by a `cd` into a directory of the worktree, so
a script of allowlisted cargo commands, with or without that `cd`, is a build in
every mode; the shell tool checks the `cd` target on disk and runs the
allowlist's checks in the directory each cargo command starts in before the
script runs. A script refused whole records the programs it held and counts as
retried once each has run, so it is not reported as a refusal never retried
when the worker ran them as argv. The user may turn on each such construct group (variables, subshells,
background jobs, input redirects, escapes, outside redirects; all off by
default) in Settings, `permissions.toml` or `--script-constructs`, never
`run.json`. Settings > Permissions also holds the rule editor
(`permission_bar::draw_rule_editor`): every list of the file (deny, ask, and the allow set's
programs, tools and paths), an add field per list checked by the loader's own per-entry check
(`kgraf_tools::permission_edit::validate`, which calls `validate_entry`, so a notice for a
Claude Code tool kGraf lacks shows as the entry is typed), and a remove button per entry. An
edit changes only its list in the file's text (`toml_edit`, so comments and other keys stay),
writes it atomically, then reads the file entry by entry again (`load_entries`), gives its valid
rules and allow set to the live control (`PermissionControl::set_rules`, `set_allow`) and
replaces the shared load result (`PermissionUi::load_result`), so a fixed file lifts a forced
Manual without a restart; a second reader then returns every program it can see, inside
substitutions and groups too, and the script is granted when a grant covers
each. What a variable runs is not seen and not checked; for such a script the
user's shell rules also match by program name in the script text, in every mode
but Bypass (decision 0094, amended 2026-10-02). A refusal is recorded against what it refused: the program a
command-level match named, or the whole tool for a skill's `allowed-tools` or
a rule with no pattern. A mode change reaches running workers and waiting
requests at once, and requests raised within 2 s of each other are answered as
one group in the box. The user's deny and ask rules and the refusal to delete
system paths hold in every mode; a run nobody attends denies what would ask.
A rule or allow-set tool must name a tool a worker can have: a built-in, a
Claude Code name read as the kGraf tool that does the same job (`Bash` as
`shell`, `Edit` and `Write` as `apply_patch`, `Read` as `read_file`), an MCP
name, or `*`. A Claude Code tool kGraf has no tool for (`WebFetch`, `Agent`
and the rest of a table) loads, matches no call, and is said as a notice that
names what kGraf uses instead and the kGraf rule to write. Any other name is
refused where the rule is read, and a rule that no worker tool of the run
matches, once the MCP servers have listed theirs, is said at run start as a
notice and traced. One per-entry check
(`kgraf_tools::permissions::validate_entry`) serves the flags, the user file
and the desktop. The command line refuses a file with any refused entry,
naming each one. The desktop reads the file entry by entry
(`SettingsLoad`): valid entries load, Settings lists each refused entry with
its reason, and a refused deny or ask entry forces Manual with the reason.
Patterns follow Claude Code's wildcards, each only widening what an existing
rule matched (decision 0094, amended 2026-10-02, rule names, and 2026-10-05).
Both read each command from every place its program may stand past grouping and
prefix words (`{`, `(`, `!`, clause keywords, `env`, `command`, `builtin`,
`exec`, `nohup`, `time`, `VAR=value`, a leading redirection), and follow a shell
behind a wrapper or started inside a script, and `eval`; one function serves the
rules, the deletion limit, the script split and the command policy's `forbid`
rules. Where a word may be an option's value or the program, both are matched,
so it errs toward refusing. A directory change kGraf is less sure of (`command
cd`, `pushd`, a `cd` in a group or a nested shell) adds a place relative paths
may resolve against and never replaces one; both script readers count a
directory changer anywhere in a command (decision 0094, amended 2026-10-02, prefix words).
In Bypass only, a worker may also write the repository's git directory
(`.git/<path>`, resolved as git resolves it; through `apply_patch`, and on macOS through any shell command, whose sandbox keeps the git configuration files read-only; elsewhere through `apply_patch` only) and kGraf's settings
(`.kgraf/run.json`, `.kgraf/worker.json`), each write behind a bounded
snapshot and an integrity check that runs before the call returns, with
`git fsck --connectivity-only` when an object file went missing and when the
attempt ends; a failed check restores the snapshot (object files from a
hard-link snapshot of the object store, copies where links are impossible)
and fails the attempt, so it is never merged. Each such write, with its check
and restore, holds the repository's lock exclusively, and every git process
kGraf runs on the repository (merges, the integration view, ref updates,
worktree add and remove, status and history reads) holds it shared: an advisory `flock` on the shared
git directory itself, so it also serializes two kGraf processes and a crashed
holder's lock is released by the kernel; waits are bounded and fail loud. The
ledger, trace, run records, transcripts and graph stay refused in every mode. In Bypass a
worker may also write `.gitattributes`, `.gitmodules` and `.githooks/...`:
ordinary tracked files under the declared write scope, merged as plain data,
with no snapshot (git history keeps every version). kGraf's own git, on the
host, runs nothing these files or the config select: every git command kGraf
runs is built by `kgraf_core::host_git`, with hooks (including config-defined
hooks), every configured filter, merge and diff driver, the file system
monitor, credential helpers, signing and every transport off, no submodule
recursion, and the inherited `GIT_*` environment removed but the identity.
Builtin attributes and the identity are kept, so results stay byte-correct; a
filtered path (git-lfs) is stored in its committed form, and a path with a
configured merge driver conflicts when both sides change it. The config listing that prepares a command runs inside the same shared hold of the repository lock as the command, so a protected write through `apply_patch` cannot change the config between the two. A Bypass shell command holds no lock, so its sandbox keeps the configuration files read-only (`config`, `config.worktree`, `info/attributes`, each worktree's `commondir`, every include target, and the directories above them); where the sandbox cannot freeze single files (Linux, Windows), a Bypass shell command keeps the git directory read-only and says why (decision 0094). The user's own git still honors everything.

and fails the attempt, so it is never merged. A shell command runs without a
lock, in parallel with other workers' commands: its git files and the object
snapshot are brought up to date before it (only what changed is read again),
and only a change found after it is checked. A protected `apply_patch` write,
and the check and restore after a changed command, hold the repository's lock
exclusively, and every git process kGraf runs on the repository (merges, the
integration view, ref updates, worktree add and remove) holds it shared: an
advisory `flock` on the shared git directory itself, so it also serializes
two kGraf processes and a crashed holder's lock is released by the kernel;
waits are bounded and fail loud. Commands overlap, so a check names every
other running command that started before the change was there: a kept
change records "changed while another worker's command also ran; cause not
exclusive", and on damage each named command's attempt fails too. The
ledger, trace, run records, transcripts and graph stay refused in every mode.
An instructions report amends the contract once; at most two re-dispatches per
contract for shortfalls. A shortfall the orchestrator would hold is verified first: an attempt that changed files goes to the verifier, and a pass merges it with the shortfall kept as a notice, while a failure, or a check that checks nothing, keeps the hold (decision 0093). With plan review on, no worker starts until the user
approves the plan in the box. On the command line, which nobody attends, a new
run with plan review on and no `--approve-plan` is refused before the project is
opened, so it creates no state, branch or trace (decision 0094, amended
2026-10-02). A contract accepted on a check that always passes (and no gate command) is reported unverified, with any refusal its worker never retried in the reason. A refusal its worker never retried beside a real check that passed is a notice and a trace `notice` event ("verified by `<check>`; the worker finished after a policy refused `<program>` and never ran it"), not an unverified mark: the check ran on the committed change whatever the worker did. The verb follows how the worker ended, so an attempt cancelled after its report is not said to have finished (decision 0093, amended 2026-10-05). A dependency missing from the local cargo cache is fetched by the host, outside
the sandbox, only with the project's consent, checked at every run start
before anything is built (decision 0087). An attempt that changed no file is
refused when a declared Rust write target holds only placeholder bodies, and
every accepted empty attempt carries a notice (decision 0086). A worker's
worktree and its build output are deleted once nothing needs them, unless the
user chose to keep every attempt's; a worktree a resume, a live worker, a
pending merge or an open contract (a handoff child continuing in it, a resolver
citing it) needs is kept either way, and each failed removal is one warning in the report (decision 0088); `kgraf clean <name>` removes a kept one from `.kgraf/worktrees` (runs) or `.kgraf/workspaces` (`kgraf worker`). The run output names each worktree a run leaves, and the worktree manager removes a hold file with its workspace and its own `.holds` and `.scratch` directories at the end of a run that kept nothing. A retry of an
unpinned contract does not repeat the route that just failed (decision 0085). A
model with no price entry is never refused (decision 0075): the ledger keeps
every call, and reports show its spend as unpriced. A planner answer that is not
the JSON asked for, or a plan that breaks a rule, is asked for again, at most
twice, each ask on the ledger. A run that ends blocked or stopped makes the
command exit non-zero.

**User interrupt.** The input box is always live. The user can stop the run,
which cancels every worker mid-stream, ends a planner call in flight, and leaves the
record resumable (a SIGINT or SIGTERM to `kgraf orchestrate` sends the same stop, then
prints the report, names each kept worktree and exits 128 plus the signal; decision 0056).
The report of a run that did not complete has one `unfinished <id>: <state>` line
for each open contract that is not held (not started and what it waits on, or
dispatched and cut off with nothing merged) and for each gate that did not pass
(decision 0056, amended). The user can also send a note, which is persisted and read by the next orchestrator call; set the
order of pending work; and answer approvals and gates. As the exception
route, the user can also address one session directly: send it a note or
redirect, answer its question, stop it or resume it.

## Key technical decisions

Each design choice is recorded as a numbered decision record. The index, with the status of
each record, is [docs/decisions/README.md](docs/decisions/README.md).

## Known trade-off: multi-agent cost

Running several agents in parallel costs more tokens than running one
competent agent on the same work. Anthropic has published that its own
multi-agent research system used roughly 15 times the tokens of a single
chat turn, and a single agent about 4 times; taking the ratio between those
two figures, rather than either alone, puts a multi-agent system's overhead
against a single competent agent at roughly 2.9 to 4.9 times, centered near
3.75 times.

kGraf's own structural defenses (shared prompt-cache prefixes, context
slices instead of whole files, disjoint writes so redundant retries are
rare) are modeled to bring that multiple down further, to roughly 2.6 times
the tokens of one competent single agent doing the same work, and roughly
3 to 5 times the dollar cost depending on which vendor and model tier the
single-agent baseline uses. These are modeled figures from the project's own
cost model, not independently measured, and confidence in the exact
multiple is low to moderate: it rests on a ratio between two
one-significant-figure estimates from a research workload, not a coding
one, and Anthropic separately notes that coding tasks have fewer
parallelizable subtasks than the research workload the estimate came from,
which would push the true multiple above this modeled figure rather than
below it.

**kGraf therefore makes no savings claim** (decision 0008). It is justified
on control, visibility, and the ability to route each part of a job to the
model that suits it, not on cost.

Decision 0012 records the trade-off this implies. Treating the goal as
redundant attempts with a verifier would let the project lean on the one
regime with strong published evidence behind it: repeated sampling with a
verifier, at reported pair-pass rates of roughly 16 to 56 percent. Division
of labor is the actual goal, that evidence does not transfer to it, and the
division-of-labor mode carries the worst measured conflict numbers instead.
The design answer is mandatory write-scope declaration plus a heavily
invested coordination channel, on the finding that a short, roughly
130-token coordination message recovered 82 to 98 percent of the ground
lost to uncoordinated parallel work.

A budget-matched, single-agent-versus-multi-agent experiment on a
repository-scale coding benchmark does not currently exist in the published
literature, and kGraf's own acceptance for this trade-off rests on real-world
use rather than on such a benchmark.
