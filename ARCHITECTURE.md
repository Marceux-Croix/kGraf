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

**Incremental reindexing compares content hashes, not just modification time
and size.** Comparing only time and size misses a change that preserves
both, and rapid agent writes are precisely that workload.

**An index write is one transaction, and a read never builds an index.** A file's content hash commits with its nodes, so a failure between the two cannot leave a file that reads as unchanged with stale or missing nodes. Only `kgraf index` creates or rebuilds the database. Every other command, and the host, opens an existing one, and stops with a message naming `kgraf index` when it is missing or was written by another schema version. A slice reads each node's lines from the file, compares the file's hash with the indexed one, and refuses an edited file. The store records a source root for each file, so code merged from a worker's worktree can be sliced before it reaches the checkout.

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

`kgraf-ingest` parses Markdown, PDF and DOCX. `kgraf-interpret` gathers the
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
    unfiltered. A subagent inherits its parent's restrictions.

A pre-tool hook can allow, ask or deny a call. The loop always holds its last
round back for an answer. Every call's trace records what it was given as
slice ids plus the prefix hash. **Not built yet:** deferring rarely used
tools behind a search tool, and an optional intent argument on each tool call
that labels the action on the graph at no extra model call.

**Built:** `ask_user_question`. The orchestrator first tries to answer from
the run record in one reserved, accounted call; the user is asked only when
it cannot. A question waits for the user a bounded amount of time (longer in
an unattended run, shorter otherwise), then returns a recorded no-answer
result that tells the worker to state its assumption instead.

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

**Loop behavior worth knowing.** A successful write starts a new stall count, so edit and test cycles, and re-reads after a change, do not end a run as stalled. Observation masking keeps the last N rounds of tool results, every block of a round together. A resume replays only calls that succeeded. A paused turn fails loud. A stop at the output ceiling gets one counted continuation round, then fails. `apply_patch` refuses a patch that names one path in two hunks, and its mismatch hints name the nearest match. A shell command's output is bounded as it is read, so a chatty command cannot grow host memory, and the command's process group is killed after a normal exit so background children do not hold the worktree.

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
the live model actually saw. kGraf's own configured credentials register
into the shared registry that backs both paths. A transcript file holds a
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
there is nothing to compact. Worker transcripts stay append-only, because
editing an earlier message invalidates every thinking block after it.

**Dispatch order is readiness, then leases, then run.** Readiness means
credentials present, MCP servers reachable, and tool discovery complete. A
worker that cannot run must never hold files another worker is waiting on.

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
repeated paid call.

**Contract checks and challenges (decisions 0029 to 0037)** are available
on the live path, off by default, opt in per run. When enabled, the planner
writes an interface contract for each place one contract uses another's
code, and each is registered in the run trace before any party dispatches.
The sandboxed runner checks it: stub-compile, vacuity against the stub and a
reference, then a handshake, one call per party on that party's own route
through the ledger. A worker challenges with a dedicated tool; one challenge
per lineage and interface is open at a time, each worker has a fixed budget
of challenges, and escalation to the user follows only a paid review.

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
the number of recipients. Such a report reaches its recipient in the next
fresh brief. A worker's own outbound message goes over a connection: open by
rule (sibling, dependency, shared read, parent and child) or by one batched,
reserved orchestrator judgment, and delivered mid-run by injection. Volume is
never refused, only paced, and kept in order.

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
lockfile the worker generated, and cache directories, are left out of the
commit, with a notice, and removed before the check, so the check runs
against the committed tree only. A check that changes committed files fails
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
own strength and gaps. **Not built yet:** a per-platform strength indicator in
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
After an accepted merge the contract's written paths are reindexed, so the
next contract's slices see them. **Not built yet:** reindexing triggered
directly by a worker's edits before its merge is accepted.

A worker builds and tests its own work. A declared allowlist of build and
test commands runs without approval in the sandbox with the network off
(decision 0084); under a write scope nothing else that can write runs. A
dependency missing from the local cargo cache is fetched by the host, outside
the sandbox, only with the project's consent, checked at every run start
before anything is built (decision 0087). An attempt that changed no file is
refused when a declared Rust write target holds only placeholder bodies, and
every accepted empty attempt carries a notice (decision 0086). A worker's
worktree and its build output are deleted once nothing needs them, unless the
user chose to keep every attempt's; a worktree a resume, a live worker, a
pending merge or an open contract (a handoff child continuing in it, a resolver
citing it) needs is kept either way, and each failed removal is one warning in
the report (decision 0088). A retry of an
unpinned contract does not repeat the route that just failed (decision 0085). A
model with no price entry is never refused (decision 0075): the ledger keeps
every call, and reports show its spend as unpriced. A planner answer that is not
the JSON asked for, or a plan that breaks a rule, is asked for again, at most
twice, each ask on the ledger. A run that ends blocked or stopped makes the
command exit non-zero.

**User interrupt.** The input box is always live. The user can stop the run,
which cancels every worker mid-stream and leaves the record resumable; send
a note, which is persisted and read by the next orchestrator call; set the
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
