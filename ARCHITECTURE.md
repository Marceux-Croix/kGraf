# kGraf Architecture

This page explains how kGraf is built. It is for a developer or reviewer who knows software and has used coding agents, but has not read the code. Decision records in [docs/decisions](docs/decisions/README.md) carry the reasoning behind each choice. Decisions are cited by number, such as decision [0054].

## 1. Overview

kGraf is a local desktop harness. One orchestrator agent directs worker agents across several model providers. A code knowledge graph, drawn as a galaxy, is the main interface.

One Rust process does everything except draw. It:

- indexes the codebase into a graph;
- runs the orchestrator and every worker as concurrent tasks;
- talks to model providers through one client trait, and to the MCP (Model Context Protocol) servers the user configured, local or hosted ([0051]);
- streams events to the shell that renders the graph.

The shell is a native Rust window with a taskbar icon and a tray icon. It draws with egui and `wgpu`, and it runs without a GPU ([0054]). kGraf is single-user and runs on the user's machine. Every network path it uses is listed in the privacy section of `docs/USAGE.md`.

This page describes what is built. Section 7 lists what is not.

```text
+------------------------------------------------------------------------+
| shell: native Rust window and tray, egui + wgpu (0054)                 |
|   galaxy graph (GPU, or a stated CPU mode)                             |
|   text surface                                                         |
|   orchestrator input box                                               |
+------------------------------------------------------------------------+
|   ^ typed events (core to UI)        commands (UI to core) v           |
+------------------------------------------------------------------------+
| Rust core                                                              |
|                                                                        |
|   orchestrator --- briefs ---> agent runtime (N workers)               |
|        ^                           |             |                     |
|        +------ message bus --------+             +--> tools            |
|                                    |                                   |
|   code graph <---- slices ---------+----> MCP servers                  |
|    + layout cache                          (local or hosted)           |
|    + write-lease registry                                              |
|        ^                               provider layer + ledger         |
|   indexer (tree-sitter)                      |                         |
|        ^                                     v                         |
|   working tree + worktrees          Anthropic / OpenAI / Google /      |
|                                     Bedrock / other routes             |
|                                                                        |
|   instructions and skills inventory (from the user's agents)           |
|   scheduler (recurring and one-shot runs)                              |
|                                                                        |
|   SQLite   graph.db, ledger.db, schedule.db                            |
|   files    .kgraf/runs/ (transcripts, traces)                          |
|            docs/agent-comms/ (agent messages)                          |
+------------------------------------------------------------------------+
```

**Figure 1.** One process holds the orchestrator, the workers, the graph and the ledger. The shell is the only separate layer. Events flow up to the shell and commands flow down.


## 2. Glossary

These terms are used throughout. Each is defined once, here.

- **Attempt**: one worker's try at one contract, in its own git worktree. A retry is a new attempt.
- **Contract**: one unit of planned work. It has an instruction, an acceptance command, the file paths it may write, and its dependencies on other contracts.
- **Gate**: a work node that runs the project's own validation commands (for example `cargo test --workspace`) against a pinned commit. The **root gate** is the gate at the top of the hierarchy. It runs once everything is built.
- **Galaxy**: the graph view. Code, agents and work items are drawn as points in clusters. Code dots are gray and sized by number of connections, in the built-in demo scene too; color marks only state (selection or match, needs input, failure).
- **Coupling cluster**: a set of contracts whose write sets overlap, that depend on each other, or whose files share a co-change count of 3 or more. The cluster cap and vendor routing treat the set as one unit.
- **Hand-back**: what a worker does at its stop point. It returns the unfinished remainder to the orchestrator, which makes it a new child contract.
- **Handshake**: a note that the worker of each of two coupled contracts writes on its own route. It states what the worker provides, what it assumes the other provides, what it assumes will not change, and what it touches. The notes are compared mechanically: a symbol written as a bare file name is read as the one stub file with that name, and a kind written right after a file ("`F` as type `Code`") is not a claim the provider must repeat. A mismatch holds the parties, and a plan held before any work goes back to the planner once. A check that still fails when no re-plan will happen (the re-plan is spent, the planner time budget refused it, or work already started) is dropped with a notice naming the interface, its finding and why no re-plan happened; its edges are counted as checks not run, and the contracts run; the verify gate still judges the work.
- **Ledger**: the record of every provider call, with its usage and cost. It is a hash chain, so edits show.
- **Plan target**: the most context a contract should need at its start. It is half the stop point.
- **Presumed work node**: a work node read from the project's own roadmap document before any plan is approved.
- **Round budget**: the number of model rounds a worker gets before kGraf checks its progress.
- **Shortfall**: a worker's report, by the `report_shortfall` tool, that policy refused something it needs or that its instructions fell short. The worker stops.
- **Sizing miss**: a count of times a contract was bigger than planned. A compaction or a round-budget extension each add one.
- **Stop point**: the context size at which a worker must hand back or compact.
- **Stub-compile check**: builds a trivial stub of a producer's interface, so a consumer's test runs against code that compiles.
- **Vacuity check**: confirms that a contract's test fails against the stub and passes against a correct reference.
- **Work node**: one item in the work hierarchy. Work nodes are distinct from code nodes and agent nodes. Doneness is derived, never stored.

## 3. From instruction to merge

1. **Instruction.** The user types one, or approves a plan read from the project's documents. A run indexes the working tree first, so the planner and workers get graph slices.
2. **Plan.** The orchestrator asks a model to split the instruction into contracts. Each contract declares the files it will write. A malformed answer is asked for again within a fixed bound.
3. **Contracts.** The orchestrator picks a route (vendor and model) for each contract and dispatches those whose dependencies are met and whose write paths are free.
4. **Attempts.** Each attempt runs a worker in its own worktree. The worker reads graph slices, edits files and runs build and test commands in the sandbox. It returns a compact result.
5. **Verify.** The verifier commits the worker's change, checks that it stayed inside the declared write scope, and runs the contract's check. Where several attempts exist, it selects one.
6. **Merge.** Verified work merges onto the run's own branch, one contract at a time. Gates run at the joins of the hierarchy. The user's checked-out branch never moves.

## 4. The governing principle and collision control

**Studies of multi-agent coding split on shared state versus disjoint state, not on single agent versus multi-agent.** Gains come from read-mostly or explicitly partitioned work. Losses come from agents writing into the same state. In one study of 33,596 pull requests, textual conflicts ran 41.7 percent between agents and 19.8 percent within one agent (table in [0009]). kGraf therefore lets **workers read widely and write disjointly**, and every concurrent writer **declares its write scope before writing** (decisions [0009], [0012]).

The primary mode is **division of labor**: different vendors work different parts of one problem at the same time. Redundant attempts with a verifier choosing a winner are a secondary mode.

A multi-agent run uses more tokens than one agent doing the same work. kGraf makes no savings claim ([0008]). It is justified by control, visibility, and routing each part of a job to a suitable model.

### Why the code graph does not predict collisions

A call graph records what code is. Collision risk depends on what changes with what, which only history shows. In measurement, the graph's verdict that two changes were disjoint was close to independent of whether they collided ([0020]). Decision [0020] made co-change (how often two files change together in version history) the collision relation, and decision [0022] stopped predicting collisions at dispatch. Co-change survives as the coupling signal in the next list.

### What controls collisions today

All of these are built.

- **Write leases** ([0048] to [0050]): exact, taken when a write set is admitted. See section 5.12.
- **Quality gates** as work nodes ([0018], [0019]).
- **Merge-conflict resolver** ([0073]): a conflict git reports at merge-back dispatches a resolver worker.
- **Speculative merge** ([0074]): while two or more contracts are in flight, a round trial-merges each worktree onto the integration tip, and each pair together, in the object database. It validates clean pairs with the gate commands. A pair that merges alone but not together is a textual conflict. A pair that merges cleanly and fails validation is a semantic conflict. The finding is traced and stored in the run record. It never delays or cancels a merge.
- **Cross-feature ordering** ([0022], [0074]): among contracts the user did not pin, the next one dispatched is the one least similar, by shared significant words, to what already runs. A found conflicting pair counts as maximally similar. It reorders contracts and never withholds one.
- **Cluster cap** ([0082]): at most two contracts of one coupling cluster run at once.
- **Routing by coupling** ([0075]): the contracts of one coupling cluster are pinned to one vendor under `RoutingPolicy::Fitness`, which is opt-in.

Speculative merge and cross-feature ordering run in the live path. A speculative round runs every 60 seconds, only while two or more contracts have a worktree in flight. It takes one work-item slot, like a gate. No user setting turns it off.

The code graph keeps its other jobs: producing context slices for agents, and modeling the interface structure the orchestrator plans against.

## 5. Components

### 5.1 Code graph

The index is both the picture the user sees and the context agents read. Every node is a code element ([0010], [0025], [0038], [0049], [0050], [0065]).

- **The graph indexes locations, not text.** A node holds a file path and a line span. Source is read from disk on demand, so a slice never serves stale source. A slice refuses a file whose hash differs from the indexed one.
- **Chunking** follows disjoint tree-sitter syntax spans, with no sliding windows. Markdown headings become section nodes.
- **A context slice** is a requested symbol plus a neighborhood sized to a token budget. A slice shows each line once. Slices of one brief share a record, so a symbol or file shown earlier is named in one line. A slice name takes the forms a planner writes: a bare or dotted name, `Type::method`, a crate or module prefix, or `path/to/file.rs::name`. A name that matches several nodes gives none, and the miss is traced with the candidates. Slice retrieval is weakest for changes that span the codebase.
- **Reindexing compares content hashes**, not only modification time and size. A write is one transaction, and a read never builds an index. Only `kgraf index` creates or rebuilds the database.
- **A walk skips installed packages**, including any directory that holds a `pyvenv.cfg`.
- **Call resolution** is by name, scoped by proximity, and bounded in fan-out ([0038]). Calls stay in their language family. Each extractor is stamped with a hash, so a changed extractor re-derives only its language. Rust, Python, TypeScript, TSX, JavaScript and Markdown use kGraf's own extraction tables. Go, Java, C, C++, Ruby, C# and PHP chunk from each grammar's `TAGS_QUERY`, and a slice marks those members as resolved by name only ([0065]).
- **Node identity** derives from the parent's id, the name, and a digest of the signature, meaning the source up to the body. A body edit does not change it. Siblings that no signature can tell apart carry an "identity unstable" flag ([0049], [0050]).

### 5.2 Co-change relation

A weighted relation computed from git history by `kgraf cochange`, and queried by `kgraf partners`. Every observed pair is kept with its count, and nothing is thresholded at index time. Commit-size filtering and recency weighting are read-time options. Its one consumer is the coupling cluster: the cluster cap and routing read pairs with a count of 3 or more ([0020], [0075], [0082]).

### 5.3 Document ingestion and interpretation

kGraf reads whatever documentation a project has and turns it into a plan model. No format or filename is required ([0013], [0064]).

- `kgraf-ingest` parses Markdown, PDF and DOCX, and reads plain-text and source-format files up to 2 MiB.
- `kgraf-interpret` ranks the corpus with a port of aider's repo-map PageRank, builds a bounded brief, and makes one accounted model call. The reading prints a plan id. Nothing is created until the user approves that id.
- The corpus supplies the verifier. A command counts as discovered only when a document contains it whole. A proposed command becomes a gate only when the user accepts it by name. Where the corpus declares no gates, the orchestrator says so and asks.
- **Presumed work nodes** ([0064]). A roadmap parser, with no model call, reads status tables, task lists and status headings into presumed work nodes. It compares the document's status word with the named code in the graph and states any disagreement. A re-read with no change writes nothing. An entry no longer listed is marked gone, never deleted. `kgraf index` and the desktop shell (when it opens an indexed project) run the read, and no token is spent.

### 5.4 Provider layer

One Rust trait with adapters behind it, written on `reqwest` and `serde` ([0006], [0041], [0039]).

- The seam is a small mandatory trait plus separate opt-in capability traits. Provider types never leave the adapter crate.
- **Accounting normalizes per protocol, not per provider**, because providers differ on whether cache tokens sit inside the input total. Every call stores six usage fields plus the raw usage response.
- **Every adapter checks that the response model equals the requested model.** The mandatory trait method takes a ledger-minted token with no public constructor, so a call outside the ledger does not compile ([0039]).
- **Vendors live in a catalog, not in code** ([0041]). "OpenAI-compatible" is two protocols: Responses and Chat Completions. The catalog also holds Anthropic, Gemini, Vertex, Bedrock Converse and Azure OpenAI routes ([0090], [0091], [0092]). `docs/MODEL-SUPPORT.md` lists every route and model kind, and says which have a recorded live exchange (VERIFIED) and which do not (BUILT, UNTESTED). Read it before relying on a route.
- **Model kinds** ([0089]): `generative`, `typed-decision`, `embedding` and `rerank`. Kind is an open name checked against a registry. Nothing calls a non-generative kind on its own. The orchestrator decides. A call for a kind a route does not serve fails loud.
- **Credentials** come from environment variables, one per route unless one key truly serves several. A missing key raises and names the variable.
- **A failed call** is written off as unbilled only when it provably billed nothing.
- **The ledger** is an event-sourced hash chain with one writer thread and an external tip anchor ([0059]). Calls with no price entry are shown as unpriced and never refused ([0080]).
- **Rate limits** use a GCRA (generic cell rate algorithm) curve per route. The limiter is shared across all kGraf processes on a machine through one SQLite file ([0072]). If it cannot be opened, the run refuses to start.
- **A buffered call's time bound scales with its output cap**, because a non-streamed call sends nothing until the answer exists.

### 5.5 Agent runtime

Workers are tasks in this process. The loop assembles context, calls the provider, dispatches tool calls, and returns a compact structured result ([0001], [0043], [0057], [0095], [0096], [0098]).

- **Tool calls run in order of conflict.** A call waits for every earlier call whose read or write access conflicts with its own. Independent calls overlap.
- **Cancellation** is probed mid-stream. A running worker can be steered: a note is stored in the session inbox before it is acknowledged, then injected at the next tool-batch boundary.
- **Worker context** has a shared prefix and a private remainder ([0043]). The prefix (system prompt, tool definitions, common graph slices) is byte-identical across workers on one vendor and model, so the fleet pays one prompt-cache write. The remainder holds the brief, the worker's own slices and its transcript.
- **Hand-back or compaction at the stop point** ([0095]). Old tool outputs are masked on every request ([0057]). At the stop point a worker hands back, and the orchestrator makes the remainder a child contract. Where the guards would refuse that child, or it could not start under the plan target, the runtime compacts the worker instead: one accounted summary call in a fixed schema, with kGraf's own state filling any gap. Earlier events stay in the append-only transcript, and a `recall_history` tool reads them. A worker compacts again only if it made progress since, and only up to a ceiling.
- **The round budget is a progress checkpoint** ([0096]). Where the loop would warn that few rounds remain, it measures the block: files that changed on net, checks that improved, and sloppy patterns such as the same error three times. Model text is never read. A progressing block earns another block. Two idle checkpoints end the attempt.
- **A turn that only announces work is not a completion.** An attempt that ends with no successful write, and whose contract declares writes, gets a notice and is told to call a tool. After two notices it fails.
- **Permission shortfalls.** A worker refused by policy reports with `report_shortfall` and stops ([0093]). The user's permission mode decides what happens ([0094]). See section 5.14.
- **Edit tools.** `apply_patch` repairs what the model's intent makes certain, and notes each repair. `replace_in_file` ([0099]) needs no patch format. Both write through one path, so the write scope, protected paths and sandbox apply the same way. Shell commands are bounded in output and time ([0084]).
- **Tools come from three places:** kGraf's graph and file tools; MCP servers (stdio, or Streamable HTTP with OAuth for hosted servers, [0051]); and skills in the `SKILL.md` format, ingested from the user's other agents ([0053]). A skill's allowed tools are enforced by the runtime, not by asking the model.
- **Questions.** `ask_user_question` first tries to answer from the run record in one accounted call. The user is asked only when that fails. An unattended run returns a recorded no-answer result at once ([0067]).
- **Secret scrubbing** ([0071]) runs on two paths. Model-facing text is redacted only against kGraf's exact-secret registry, except build and test output, which also gets the pattern rules ([0084]). Persisted and displayed text always gets the gitleaks-derived pattern rules.


### 5.6 Orchestrator

The single director ([0004]). It splits an instruction, or an approved plan, into contracts. It routes each contract, spawns and retires workers, and answers the user.

- **Delegation is asynchronous.** The orchestrator issues a brief, takes a handle and keeps working. Worker reports follow a five-kind delivery policy so a live graph does not flood the user. The fifth kind, contract-inadequate, is the only one that fans out to peers ([0027]).
- **The orchestrator holds no transcript.** Its state is the run record, which compaction never rewrites. Each call is built from that record and the verbatim original instruction ([0057]).
- **Dispatch order is readiness, then leases, then run.** A worker that cannot run never holds files another worker waits on.
- **A route that fails at the transport is left behind** for a cooldown that doubles with each fault, and the attempt does not count against its contract. After three faults in a row with no other route, the contract is held ([0075], [0085]).
- **Spend is recorded, never limited** ([0080]).
- **A killed run resumes** from its record with no repeated paid call ([0056]).
- **No local decider model ships** ([0042] to [0047]). Routing and recursion decisions are frontier calls or fixed rules.
- **A malformed structured answer is asked again within one bound** (twice). Each try is its own ledger call. A cut-off answer that stops right after a complete value is closed mechanically before it is asked again ([0030], [0039]).
- **Planning** can add contract checks, which are off by default and opt in per run ([0029], [0030], [0031], [0035], [0036], [0037], [0076], [0078], [0079], [0081]). With checks on, a second planner call writes an interface contract wherever one contract uses another's code. The sandboxed runner checks it in three steps: the **stub-compile check**, the **vacuity check**, then the **handshake**, in which each party states what it expects, on its own route. The planner is given the project's real names and signatures and is refused when it invents them. A plan that no worker could verify is refused with the cause named, and asked again within a bound.


### 5.7 Message bus

The bus carries briefs, results and direct worker-to-worker messages. It records all of them, so selecting an agent shows every message it sent or received ([0026], [0067]).

- Delegation is guarded by roster membership, cycle rejection, depth and size caps, and a policy table.
- Each pair of agents maps to one conversation file under `docs/agent-comms/`, named from the two ids in canonical order. The file is committed to the integration branch and indexed, so a message is a node on the change log.
- **Text from another agent reaches a worker as data**, inside an escaped `agent_message` block, never as an instruction.
- A message to a running worker is injected at its next round boundary. The sender's tool result says "queued", never "delivered". Only the recipient's transcript shows delivery.
- A note not delivered when a run ends is traced `undelivered` and reported to its sender.

### 5.8 Persistence and event spine

Three SQLite databases per project, under `.kgraf/` ([0007]):

- `graph.db`: files, nodes, edges, layout, co-change, leases, work nodes, facts, schedules, run records and the change log;
- `ledger.db`: the call ledger;
- `schedule.db`: the claim table for scheduled runs.

**Only facts are stored. Display status is derived at read time.** Triggers on the graph tables append to the change log, and one poller fans it out to the interface, so no write escapes the interface. Worker transcripts and run traces are files under a per-run directory. The state directory is added to the repository's local exclude file, never to the tracked `.gitignore`. Merge-back refuses an attempt that touches it.

### 5.9 Instructions and skills

`kgraf-instructions` reads, into one inventory, the instruction files and `SKILL.md` skills that the user's other coding agents already use, plus any folder the user imports ([0053]). Instruction layers keep order and provenance. Byte-identical skills merge. Two different skills with one name are a conflict the user resolves. Every malformed file is reported.

### 5.10 Scheduled runs

An orchestrator run can start on a recurrence rule or at an instant ([0052]). A schedule belongs to a work node, and each run is history on that node. Due occurrences are claimed atomically, and readiness is checked before capacity. A missing credential disables the schedule and says why. The app keeps a tray process so schedules fire with the window closed.

### 5.11 Desktop shell

One native Rust binary ([0054]): `winit` for the window, `tray-icon`, egui for panels and `wgpu` for the galaxy. Typed events flow from core to UI, and commands flow back. Both sides are Rust, so no cross-language boundary can drift. A webview's WebGL falls back to software without telling the app. `wgpu` names its adapter, so CPU rendering is a stated, detectable mode.

`kgraf-shell` (binary `kgraf-app`) holds:

- the window, panels, tray, and adapter selection with the adapter type shown;
- `theme.rs`: tokens for the light and dark themes, and the five semantic colors under the chosen color-vision mode;
- **color-vision modes** ([0070]): protanopia, deuteranopia, tritanopia and achromatopsia, each with its own table of the five semantic colors, chosen to stay distinct under a simulation of that deficiency while keeping WCAG AA contrast. The setting takes effect on the next frame, and the galaxy reads the same palette;
- a settings dialog with five tabs (General, Runs, Permissions, Appearance, Providers);
- `projects.rs` ([0066]): a workspace of open projects, each with its own host, scheduler, graph view and runs;
- `activity.rs` ([0063]): one live activity card of change-log events, each routing to its node.

The UI rules are in [docs/STYLE_GUIDE.md](docs/STYLE_GUIDE.md).

### 5.12 Work hierarchy and scheduling

Work is a recursive hierarchy of work nodes. A part decomposes until one piece fits a single worker's brief ([0014], [0015], [0016], [0017]).

- **Doneness and blocking are derived, never stored** ([0007]). A node is blocked by an unfinished dependency, a denied write scope, or a worker reporting stuck.
- A node is **eligible** when its dependencies are met and **admissible** when the leases on its write set are free. The two checks are separate, so the interface can tell a blocked node from a colliding one.
- **Autonomy is bounded.** One cap limits concurrent work items and a separate ceiling limits live agents. Both refuse a spawn before it starts. A node held by the cap is a third visible reason.
- **Run limits are the user's global settings** (rounds per worker, reply cap, assumed context window). A command-line flag outranks a setting, which outranks the default.
- **Scheduling has two modes.** A play control hands the orchestrator everything. Or the user selects nodes and an order, and the orchestrator completes the partial order. Admission control binds in both.
- **Write leases** ([0048] to [0050]) are one registry keyed by path. A whole write set is taken atomically in sorted order, so deadlock cannot be constructed. A lease type has no public constructor, so an unleased write does not compile. A blocked acquisition returns every conflict with its holder, and the orchestrator arbitrates. Sub-file leases keyed on stable node ids exist because file leases over-serialize ([0049]).

### 5.13 Galaxy graph UI

Layout is computed in the Rust core and cached, so the renderer never runs a force simulation ([0058], [0010]). The renderer is an instanced `wgpu` renderer, with shader techniques taken from rerun's `re_renderer`. A **CPU mode** draws point sprites and edges on selection only, and does not animate when nothing changed.

- The layout is anchored: directory-ring seeding, depth on the third axis, and a disc per cluster that grows in power-of-two size buckets. A node moves only when its cluster crosses a bucket or its call depth changes.
- Code nodes are gray and sized by degree, the count of their edges (STG-DRAW-2 in the style guide). Only the demo scene draws the star colors that the layout cache also stores. Agents and work items have their own shapes. Selecting an agent dims everything unrelated.
- Edge intensity dims with the edge count, because edges, not nodes, wash out a large view.
- **Folding** ([0068]). Children fold into parents: a chunk under its containing parent, a file under its directory. No node shows more than 64 children, and the automatic cut draws at most 1,000 hierarchy nodes. The cut keeps its previous shape while it still fits, so one change moves the view by about the size of the change. Work nodes fold too and are never opened by the automatic cut. A click expands a folded node.
- The galaxy is the only work surface. There is no board or list view ([0003]).
- `kgraf-galaxy` holds the layout, the renderer, and a live feed from the change log. The interaction layer sits on the same crate: the orchestrator's input box, approvals answered in place, play, stop and order, and search whose result is a graph selection ([0032], [0034]).

### 5.14 Worktrees, merge, gates and permissions

Each writing worker gets an isolated git worktree, driven by the git CLI. **Worktrees merge back continuously** as workers finish, so conflicts arrive one at a time ([0018]). Each worktree builds into its own target directory, seeded by a clone ([0060]). A worktree is deleted once nothing needs it unless the user chose to keep it ([0088]).

**Quality gates are work nodes** ([0019]). One attaches to every node with children. Gates run the validation commands discovered from the project's documents and build files, against a pinned commit, so merging never freezes. A gate that fails halts new dispatch without canceling work in flight, and produces a remediation node. A repository with no discoverable validation command gets no gate, and the orchestrator says so.

**A work node is done when its code exists and has passing tests.** Workers write the tests. The runner is discovered from the corpus.

- **A failing gate below the root** reruns at the commit the run started from. If it fails there too, it is **inconclusive**: it halts nothing, and the root gate judges. The root gate is never inconclusive.
- **A contract is accepted on its own tests.** For a recognized whole-suite runner, each attempt runs only the tests that belong to its declared files. The whole command is added as a root gate. A failure that also fails at the dispatch base, while other contracts are open, is deferred to the root gate ([0018]).
- An attempt that changed none of its declared files is refused, unless an earlier merge already delivered them.

**The verifier judges write scope before the check runs.** It commits the worker's change, judges scope on that commit, and merges only it. Lockfiles, caches, build output and compiled files are left out of the commit with a notice. A check that changes committed files fails verification. Host writes never follow a link ([0083]). An attempt that changed no file is refused when a declared Rust write target holds only placeholder bodies ([0086]).

**Permission modes** ([0093], [0094]) are one live setting that only the user sets:

- Manual asks the user.
- Accept edits grants file edits.
- Auto grants the user's allow set.
- Orchestrator decides grants or refuses each request without asking.
- Bypass grants everything, with limits.

The user's deny and ask rules and the refusal to delete system paths hold in every mode. A run nobody attends denies what would ask. A command that downloads packages is refused in every mode outside a disposable remote environment. Dependencies are fetched on the host only with the project's consent ([0087]). In Bypass only, a worker may write the repository's git directory and kGraf's settings, behind a snapshot and an integrity check that restores on failure. kGraf's own git commands run with hooks, filters, drivers and credential helpers off. The ledger, trace, run records, transcripts and graph stay refused in every mode.

### 5.15 Sandboxing

Each platform uses native primitives and needs no administrator rights, except Windows setup ([0011], [0084], [0097]). Each backend reports its own strength and gaps.

- **Linux**: Landlock, seccomp and namespaces, ported from codex-rs. Cargo is granted read access to trusted ancestor files above a worktree only when the caller names a checkout root, and only for files owned by this user or root and not writable by others.
- **macOS**: Seatbelt.
- **Windows**: every sandboxed command runs as a dedicated local sandbox user, created once by `kgraf sandbox setup`, which asks for administrator rights ([0097]). Writes and reads are confined by access-control entries. The network is blocked by Windows Firewall rules plus Windows Filtering Platform (WFP) loopback filters. DNS has no working filter layer on Windows Server 2025, and setup reports it. A launcher service lets cargo and rustc make the named kernel objects they need. If `setup` has not run, Windows falls back to the restricted-token sandbox of decision [0011], which does not block the network. Gates and checks then run with the network open and a notice says so. Steps and status are in `docs/USAGE.md` and `docs/WINDOWS-TESTING.md`.

Workers build and test against dependencies the user installed. Installed trees (`node_modules/`, `.venv/`, `vendor/`) are linked into the worktree read-only. A declared allowlist of build and test commands runs without approval, with the network off ([0084]). With the user's consent, a missing install can run in the sandbox through kGraf's egress proxy ([0087]).

### 5.16 Diagnostics

Local-only usage counts and errors. `kgraf-diag` has no network code ([0069]). One capped per-user JSONL file records launches, runs, workers (provider and model names, never credentials), feature use, and every error under a stable code. No secrets, file contents, prompts or completions reach it. Credentials in error text, such as a credentialed URL or an API key in a provider's error body, are redacted first. Paths inside the project are written relative to it, and other paths are hashed with a keyed BLAKE3. Recording never blocks, and a full channel drops the event and counts it. `kgraf diag show` prints a summary. `kgraf diag export` writes a re-scrubbed copy the user can share.

## 6. Crate map

| Crate | Holds |
|---|---|
| `kgraf-core` | ids, the token-usage normal form, money, budgets, the shared color palette |
| `kgraf-graph` | chunker, SQLite store, queries, slices, co-change, leases, work nodes, repo-map ranking |
| `kgraf-provider` | the provider seam, the ledger, the streaming decoder, rate limits |
| `kgraf-provider-anthropic`, `-openai`, `-bedrock`, `-google` | one adapter crate per wire protocol |
| `kgraf-fleet` | the route catalog and credentials |
| `kgraf-decide` | the typed-decision seam; no weights ship |
| `kgraf-instructions` | the instruction and skill inventory |
| `kgraf-runtime` | the worker loop |
| `kgraf-tools` | worker tools and command policy |
| `kgraf-secrets` | secret detection and redaction |
| `kgraf-mcp` | the MCP client (`kgraf-mcp-test-server`: tests only) |
| `kgraf-session` | transcript, approvals, skills at run time |
| `kgraf-capacity` | the per-machine parallelism maximum |
| `kgraf-orchestrator` | decomposition, contracts, checks, gates, run record, scheduler loop |
| `kgraf-host` | run construction shared by the command line and the shell |
| `kgraf-schedule` | recurrence and the claim table |
| `kgraf-ingest`, `kgraf-interpret` | corpus parsing and the interpretation pass |
| `kgraf-sandbox` | worktrees, sandboxes, merge-back, per-worktree build directories |
| `kgraf-galaxy` | layout and renderer |
| `kgraf-shell` | the desktop app (`kgraf-app`) |
| `kgraf-cli` | the `kgraf` command |
| `kgraf-diag` | local-only usage counts and errors |
| `kgraf-bench-briefs` | a per-model brief-following bench ([0075]) |
| `kgraf-tbench-bridge` | benchmark only: routes a worker's shell commands into a Terminal-Bench container; not shipped |
| `kgraf-soak` | dev only: many runs on one host in one process; not shipped |
| `kgraf-crosslimit-hammer` | dev only: a second process against the shared rate-limit store, for tests; not shipped |
| `kgraf-wintest` | dev only: runs the cross-built Windows test binaries on a Windows machine without Rust and writes one report; not published or packaged |

## 7. Not built yet

- Co-change: sub-file granularity, recording kGraf's own observed collisions, and a cold-start policy.
- Enforcement of corpus constraints that name no path. A constraint that names forbidden paths strikes matching writes from the plan. Any other constraint only reaches worker instructions.
- Sticky inherited fallback chains for per-worker model selection.
- A tool search a worker can use to find deferred tools (the registry can hold them, but no search tool ships), and an intent argument on each tool call.
- A gate that refuses to build without an agreed roadmap on every path. An instruction given directly dispatches without one.
- A runs table. The run record has a `parent_run` field that nothing sets yet, so delegation is not a walkable tree.
- Edges from a work node to the code nodes it is expected to change.
- Edges re-routed between folded parents ([0068]); a bloom pass, and node glow boosted by channel dominance.
- Call traces on nodes.
- Execution runtimes other than git worktrees.
- A sandbox strength display. Each backend reports its strength as data (`IsolationStrength`), and no surface shows it.
- Reindexing triggered by a worker's edits before its merge is accepted.

[0001]: docs/decisions/0001-native-agent-loop.md
[0003]: docs/decisions/0003-graph-is-the-only-surface.md
[0004]: docs/decisions/0004-single-orchestrator.md
[0006]: docs/decisions/0006-provider-layer-shape.md
[0007]: docs/decisions/0007-persistence-and-event-spine.md
[0008]: docs/decisions/0008-cost-position.md
[0009]: docs/decisions/0009-disjoint-state-write-discipline.md
[0010]: docs/decisions/0010-galaxy-ui-stack.md
[0011]: docs/decisions/0011-sandboxing-per-platform.md
[0012]: docs/decisions/0012-heterogeneous-specialization-primary.md
[0013]: docs/decisions/0013-document-corpus-seeds-the-graph.md
[0014]: docs/decisions/0014-work-node-hierarchy-and-execution-control.md
[0015]: docs/decisions/0015-dynamic-graph-and-local-perturbation.md
[0016]: docs/decisions/0016-scheduler-scope-is-the-whole-work-graph.md
[0017]: docs/decisions/0017-concurrency-limits-and-settings.md
[0018]: docs/decisions/0018-continuous-merge-and-gate-nodes.md
[0019]: docs/decisions/0019-gates-at-hierarchy-joins-and-tests-in-done.md
[0020]: docs/decisions/0020-cochange-is-the-collision-relation.md
[0022]: docs/decisions/0022-stop-predicting-lock-at-runtime.md
[0025]: docs/decisions/0025-scoping-depth-and-chunk-size.md
[0026]: docs/decisions/0026-agent-communication-is-durable-prose.md
[0027]: docs/decisions/0027-contracts-are-amended-or-superseded.md
[0029]: docs/decisions/0029-agents-may-challenge-a-contract.md
[0030]: docs/decisions/0030-mechanical-classification-and-early-detection.md
[0031]: docs/decisions/0031-closing-the-vacuity-and-handshake-holes.md
[0032]: docs/decisions/0032-search-is-primary-navigation.md
[0034]: docs/decisions/0034-one-text-surface.md
[0035]: docs/decisions/0035-challenge-escalation.md
[0036]: docs/decisions/0036-what-counts-as-a-clear-error.md
[0037]: docs/decisions/0037-rejections-name-their-artifact.md
[0038]: docs/decisions/0038-call-resolution-is-proximity-scoped-and-fanout-bounded.md
[0039]: docs/decisions/0039-accounting-is-enforced-by-an-unforgeable-call-token.md
[0041]: docs/decisions/0041-openai-compatible-is-two-protocols-and-vendors-live-in-a-catalog.md
[0042]: docs/decisions/0042-a-decider-seam-cpu-first-wire-format-targeted-calibration-gated.md
[0043]: docs/decisions/0043-one-shared-prefix-many-private-slices.md
[0047]: docs/decisions/0047-the-local-decider-question-was-answered-and-the-seam-is-rescoped.md
[0048]: docs/decisions/0048-deadlock-is-prevented-by-the-registry-and-arbitrated-by-the-orchestrator.md
[0049]: docs/decisions/0049-file-leases-over-serialize-and-sub-file-leases-need-stable-identity.md
[0050]: docs/decisions/0050-sub-file-leases-are-keyed-on-hierarchical-node-ids.md
[0051]: docs/decisions/0051-single-user-and-hosted-mcp-servers-are-allowed.md
[0052]: docs/decisions/0052-scheduled-runs-are-in-scope-with-a-background-process.md
[0053]: docs/decisions/0053-instructions-and-skills-are-ingested-from-the-user-s-existing-agents.md
[0054]: docs/decisions/0054-a-native-rust-shell-that-runs-without-a-gpu.md
[0056]: docs/decisions/0056-long-running-tasks-resume-from-a-run-state-record.md
[0057]: docs/decisions/0057-compaction-never-rewrites-and-runs-re-ground-on-the-original-instruction.md
[0058]: docs/decisions/0058-the-galaxy-layout-stays-solver-free.md
[0059]: docs/decisions/0059-the-ledger-is-an-event-sourced-hash-chain-with-an-external-tip.md
[0060]: docs/decisions/0060-each-worktree-builds-into-its-own-target-directory.md
[0063]: docs/decisions/0063-one-live-activity-card-routing-back-to-the-graph.md
[0064]: docs/decisions/0064-presumed-work-nodes-from-the-roadmap.md
[0065]: docs/decisions/0065-calls-stay-in-their-language-extractors-are-stamped-a-tags-tier.md
[0066]: docs/decisions/0066-a-project-switcher-in-the-guide.md
[0067]: docs/decisions/0067-every-agent-is-an-addressable-session.md
[0068]: docs/decisions/0068-large-graphs-fold-children-into-parents.md
[0069]: docs/decisions/0069-local-diagnostics-clarifies-no-telemetry.md
[0070]: docs/decisions/0070-color-vision-modes-live-only-in-kgraf.md
[0071]: docs/decisions/0071-transcript-secret-scrubbing.md
[0072]: docs/decisions/0072-cross-process-rate-limits-sqlite-gcra.md
[0073]: docs/decisions/0073-merge-conflicts-get-a-resolver-worker.md
[0074]: docs/decisions/0074-speculative-merge-reports-and-never-delays.md
[0075]: docs/decisions/0075-routing-by-coupling-and-measured-capability.md
[0076]: docs/decisions/0076-consumers-write-a-test-of-what-they-expect.md
[0078]: docs/decisions/0078-handshake-mismatch-diagnosis-and-the-checks-default.md
[0079]: docs/decisions/0079-a-cross-vendor-review-at-the-interface.md
[0080]: docs/decisions/0080-kgraf-does-not-limit-spend.md
[0081]: docs/decisions/0081-stubs-land-on-the-integration-branch-first.md
[0082]: docs/decisions/0082-at-most-two-coupled-contracts-in-flight.md
[0083]: docs/decisions/0083-host-writes-never-follow-a-link-and-merge-back-holds-the-declared-writes.md
[0084]: docs/decisions/0084-workers-run-an-allowlist-of-build-and-test-commands-in-the-sandbox.md
[0085]: docs/decisions/0085-retry-route-diversity.md
[0086]: docs/decisions/0086-empty-attempts-checked-against-a-placeholder.md
[0087]: docs/decisions/0087-dependencies-fetched-on-the-host-with-consent.md
[0088]: docs/decisions/0088-worker-worktrees-deleted-after-use-unless-kept.md
[0089]: docs/decisions/0089-models-of-every-kind-are-routes.md
[0090]: docs/decisions/0090-google-adapters.md
[0091]: docs/decisions/0091-bedrock-converse-adapter.md
[0092]: docs/decisions/0092-azure-openai-transport.md
[0093]: docs/decisions/0093-a-worker-reports-a-shortfall-and-the-orchestrator-grants-within-the-operator-ceiling.md
[0094]: docs/decisions/0094-permission-modes-bypass-limits-grouped-requests-and-plan-review.md
[0095]: docs/decisions/0095-a-worker-that-cannot-hand-back-is-compacted-once-by-the-runtime.md
[0096]: docs/decisions/0096-the-round-budget-is-a-progress-checkpoint.md
[0097]: docs/decisions/0097-windows-commands-run-as-a-dedicated-sandbox-user.md
[0098]: docs/decisions/0098-one-reasoning-level-setting-covers-every-model-call.md
[0099]: docs/decisions/0099-replace-in-file-is-an-edit-tool-that-needs-no-patch-format.md
