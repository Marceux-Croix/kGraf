# kGraf usage guide

The full command reference for the `kgraf` command line and the `kgraf-app` desktop
window. The [README](../README.md) has the short version.

## Build

```
cargo build --workspace
cargo test --workspace
```

The workspace needs Rust 1.95 or later. It builds two programs: `kgraf` (the command
line, `crates/kgraf-cli`) and `kgraf-app` (the desktop shell,
`crates/kgraf-shell`).

## Set up a project

Workers build and test with the network off (decision 0084), so a run needs the project's
dependencies in the local cargo cache before it starts. Answer once per project whether
kGraf may fetch them (decision 0087):

```
kgraf deps allow     # fetch from crates.io whenever a run needs it, without asking again
kgraf deps status    # the stored answer, and whether the dependencies are ready offline
kgraf deps fetch     # fetch now, once
kgraf deps refuse    # never fetch; a run that needs a fetch stops before it starts
kgraf deps reset     # forget the answer; the next run that needs a fetch asks
```

- The answer is kept in `.kgraf/dependency-fetch.json`, outside the repository's tracked
  files. It is per project.
- At every run start (the command, scheduled and tray-fired runs, the desktop app) kGraf
  checks offline whether the dependencies are ready (about a third of a second on a large
  workspace). Only when one is missing does the answer matter: allowed runs `cargo fetch` on
  this machine, outside the workers' sandbox, and says which index it contacted; refused
  stops the run with what to do.
- With no answer, `kgraf orchestrate` at a terminal asks once (once for this run, always for
  this project, not now, never for this project), and the desktop app shows the question
  as a card (Allow once, Always for this project, Not now). With nobody there to answer (a
  scheduled run, a script), the run stops before it starts and says how to answer; a
  schedule is turned off with that reason until you answer and run `kgraf schedule enable`.
  kGraf never fetches unasked.
- `--fetch-deps` and `--no-fetch-deps` on `orchestrate` and `schedule run-now` answer for
  one run and store nothing.
- A worker that adds a crates.io dependency gets it fetched when the project allows
  fetching; otherwise its build fails and says why. At most 8 fetches and 32 checks per run
  for its workers, and 3,000 packages per fetch; a fetch still running stops when the run
  ends. A refusal you store during a run (`kgraf deps refuse`) applies to the next worker
  build, even in a run started with `--fetch-deps` or "Allow once".
- Only crates.io. A git dependency or another registry in any `Cargo.toml` of the project,
  a workspace member or not, stops the fetch: run `cargo fetch` yourself if you trust it.
  `cargo fetch` downloads and unpacks; it runs no build script. kGraf's fetch never uses a
  git transport, so a crates.io index you configured as a git index cannot be fetched by it.
- With no `Cargo.toml` at the project root but one below it, nothing is checked or fetched,
  and each run start says so.
- Workers cannot merge changes to a `.cargo` folder or a `rust-toolchain` file: make those
  by hand.
- Each fetch is a network event with no tokens; `kgraf diag show` counts them.

## The code graph

Point it at a repository, including this one:

```
cargo run -p kgraf-cli -- index
cargo run -p kgraf-cli -- symbol slice_for_symbol
cargo run -p kgraf-cli -- slice slice_for_symbol --budget 1500
cargo run -p kgraf-cli -- query 'kind=function -calls-> name=stable_id'
cargo run -p kgraf-cli -- cochange --commits 500
cargo run -p kgraf-cli -- partners ROADMAP.md
```

`-C <dir>` points any command at another repository.

Only `kgraf index` creates or rebuilds the index. `symbol`, `slice`, `query`, `partners` and
`demand` open it read-only, and every other command opens an existing index without
rebuilding it. Where there is no index, or it was written by another schema version, they
stop and say to run `kgraf index`. A slice reads each node's lines from the file, and it
stops with an error when the file changed after it was indexed; run `kgraf index` first. A
name that matches more than one node is refused with the candidates listed (an exact
qualified name still picks its one node).

An index pass skips `target`, `node_modules`, `.git` and similar tool output at any depth,
and `build`, `dist`, `venv` and the like only at the top level, so `src/build/` is indexed.
It prints the excluded directories, files over 2 MiB, files whose bytes are not UTF-8, and
any symbolic link or unreadable file whose indexed rows it kept.

The graph chunks Rust, Python, TypeScript, TSX, JavaScript and Markdown with kGraf's own
rules, and Go, Java, C, C++, Ruby, C# and PHP from each tree-sitter grammar's own tags
query (the tags tier; a slice marks those members as resolved by name only). A call links
only to a definition in its own language family. When the extractor for a language
changes, the next index re-derives that language's files and says so.

## What the roadmap says remains: presumed work nodes

```
cargo run -p kgraf-cli -- index                              # also reads the roadmap
cargo run -p kgraf-cli -- plan                               # list presumed work and its state
cargo run -p kgraf-cli -- plan --from-roadmap --source docs/milestones.md
```

Every `kgraf index` reads the project's roadmap documents (ROADMAP, TODO, PLAN,
MILESTONES, BACKLOG or TASKS, `.md` or `.markdown`, in the root or `docs/`) into
**presumed** work nodes, with no model call: status tables, task lists and status
headings. Each node shows presumed done, partly done or remaining, derived from what the
document claims and whether the code it names exists, and says where the two disagree.
Re-running updates the same nodes; it never duplicates them, never deletes them, and never
overwrites a node you changed. When an approved plan takes an item over, the presumed node
keeps its identity and the galaxy draws the running work on it. `--no-presume` skips the
read; `kgraf plan --from-roadmap` with no roadmap found is an error.

## Providers, spend and the ledger

Set a route's key **in your own shell**; it is never read from a file:

```
export KGRAF_KIMI_API_KEY=...        # the route's own variable; never paste a key into a chat
cargo run -p kgraf-cli -- providers
cargo run -p kgraf-cli -- call --provider kimi --model <model-id> --record kimi.json
cargo run -p kgraf-cli -- spend --prices prices.json
cargo run -p kgraf-cli -- unreconciled
cargo run -p kgraf-cli -- unreconciled resolve <call-id> --input 120 --output 30
cargo run -p kgraf-cli -- unreconciled resolve <call-id> --no-charge
cargo run -p kgraf-cli -- ledger verify
cargo run -p kgraf-cli -- demand
```

Every model call goes through the ledger. `spend` totals it (in dollars with a price
table; `total unpriced` when no recorded model has a price), `unreconciled` lists calls
whose spend is not settled, `unreconciled resolve` closes one with the usage the provider's
own invoice shows (or `--no-charge` when the provider confirms it billed nothing), `ledger
verify` walks the
hash chain, and `ledger redact <call-id> --reason ...` removes a payload captured by
accident while the chain still verifies. `demand` reports induced demand: whether
runs offered workflow templates started more work than runs without. It claims no saving.

## A worker, and an orchestrated run

```
cargo run -p kgraf-cli -- worker --provider kimi --model <model-id> --brief brief.txt
cargo run -p kgraf-cli -- orchestrate --route kimi:<model-id> --route openai:<model-id> \
    --instruction task.txt --prices prices.json
cargo run -p kgraf-cli -- orchestrate --resume <run-id> --route kimi:<model-id> \
    --prices prices.json
```

- `worker` runs one worker in its own worktree under `.kgraf/workspaces/` and commits
  its work to `refs/kgraf/attempts/<worker>-<n>`; it never merges.
- `orchestrate` decomposes the instruction into contracts, dispatches workers across the
  routes, verifies each attempt with `--verify` (default `cargo test --workspace`), runs
  quality gates (discovered, or `--gate-command`), and merges verified work onto its own
  branch `kgraf/<run>`. Your checked-out branch never moves.
- kGraf sets no spend limit (decision 0080): set one in your provider's console. Every call
  is still on the ledger, and `kgraf spend` totals it. The concurrency caps are `--max-items`
  (default 4) and `--max-agents` (default 8).
- A model the price table does not list still runs. The ledger keeps every call, and the
  report and `kgraf spend` name its spend as unpriced (a local model as local, unpriced),
  never as zero dollars. Routing never refuses or skips a route for lacking a price.
- `orchestrate` exits non-zero unless the run completed: a run that ends blocked or
  stopped is a failure to a script or a scheduler.
- A planner answer that is not the JSON asked for, or a plan that breaks a rule, is asked
  for again (at most twice, each ask on the ledger) with the refusal as a note, before the
  run stops.
- `--rate-limit ROUTE=RPM[/BURST][@MAX_WAIT_MS]` limits one route's call rate; a call that
  would wait too long fails loudly.
- Every run indexes the working tree before it starts (incremental: about 65 ms warm and
  about 7 seconds cold on an 800-file repository), and reindexes a contract's files after
  its merge, so workers read graph slices without a separate `kgraf index`. A graph that is
  still empty is reported as a notice.
- An attempt that changes no file is accepted only when its check passes, and it is always
  reported. It is refused when a declared Rust write target holds only `todo!` or
  `unimplemented!` bodies (decision 0086; `unreachable!` is not a marker), and an attempt
  that did change files is refused when a declared target it left untouched held only such
  bodies before the run.
- Workers can build and test their own work (decision 0084). The build and test allowlist
  runs without approval, inside the sandbox with the network off: by default `cargo build`,
  `cargo check`, `cargo test`, `cargo clippy` and `cargo fmt --check`, each under argument
  rules (for example, no `--manifest-path` outside the worktree, no `--config`, no `-Z`,
  no `--target-dir`), plus the run's own `--verify` and `--gate-command` commands exactly
  as configured. Set `build_commands` in `.kgraf/worker.json` to change the list (`[]`
  turns it off). A worker with a declared write scope (every orchestrated attempt) runs
  these and no other shell command that can write; the verifier still refuses any commit
  that changes a file outside the declared writes. The list is on unless `build_commands`
  says otherwise, for attended workers too, and a cargo command that breaks an argument rule
  is refused outright, not asked about.
- While a worker's worktree has a `.cargo/config.toml`, `.cargo/config`, `rust-toolchain` or
  `rust-toolchain.toml` that differs from the one it started with, every allowlisted command
  is refused: those files can change what cargo runs whatever its arguments say.
- On macOS a sandboxed build cannot read the rest of the checkout it runs in (only its cargo
  files), so a `.env` at the checkout root stays private. Every value in that `.env` is also
  removed from what a worker's model sees, and build and test output is scrubbed of
  key-shaped strings before the model sees it.
- The sandbox cannot download or unpack crates. A run checks that the dependencies are in
  the local cargo cache before it starts, and fetches them outside the sandbox only with the
  project's consent (see [Set up a project](#set-up-a-project)).
- `--shell-timeout` (on `worker`, `orchestrate` and `schedule run-now`) bounds one worker
  shell command, in seconds; `shell_timeout_secs` in `.kgraf/worker.json` sets it for the
  project; without either it is 600. A command killed at the timeout returns its output so
  far, marked `[killed after N s]`.
- `--resume <run-id>` continues a killed run from its record without repeating a paid call.
  With it, `--retry-gate <gate>` or `--waive-gate <gate>`, plus `--gate-note`, decides a
  held gate.
- `--interactive` reads notes and `?` status questions from stdin while the run goes.
- `--gate-timeout` (1800 seconds) bounds each gate command and each run of the `--verify`
  command. On macOS and Linux a gate command also runs under a CPU-time limit of twice
  that, so a process that escapes its process group cannot spin without limit.
- Interface contracts are experimental and off by default. `--contract-checks` turns them
  on (`contract_checks` in `.kgraf/run.json`). With them on, where one contract uses
  another's code, the planner also writes an interface contract: a stub, a reference, the
  consumer's expectations and a test. Before the first worker starts, the sandbox checks
  it: the consumer and the test compile against the stub, the test fails against the stub
  and passes against the reference, and the two parties' handshakes agree. A contract that
  fails is held, with the file that failed named. A contract whose compiler or test runner
  is not installed is held too, with the program named. A worker can challenge a contract
  with the `challenge_contract` tool; the answer names the artifact that settled it, or
  says it is judgment.
- `--check-timeout` and `--check-compile-timeout` (600 seconds each) bound one check run.
  `--planner-max-tokens` (8192) is the output limit of each orchestrator call. An answer
  cut off at that limit fails and names the flag; interface contracts carry whole files,
  so they may need more.
- One Rust interface costs about 12.5 seconds of checks and one Python interface about
  5.4 seconds on a small project. The run report says whether checks were on.

## Start from the project's own documents

```
cargo run -p kgraf-cli -- orchestrate --corpus --route kimi:<model-id> --prices prices.json
cargo run -p kgraf-cli -- orchestrate --corpus --correction "the export is CSV, not JSON" ...
cargo run -p kgraf-cli -- orchestrate --corpus --approve <plan-id> --accept-command "cargo test" ...
```

`--corpus` reads the repository's documents and code, prints the plan it read with a plan
id, and stops; nothing is created in the repository. `--correction` reads again with your
correction. `--approve <plan-id>` runs that stored plan with no new model call; a proposed
validation command becomes a gate only when you name it with `--accept-command`. In an
empty directory, give `--instruction` too. The reading's work items become presumed work
nodes at once (the tokens are the reading's, already in the ledger), except those a
roadmap node already stands for; approving the plan adopts them.

## MCP tools, stranded worktrees and scheduled runs

```
cargo run -p kgraf-cli -- allow mcp__<server>__<tool> --note "why"
cargo run -p kgraf-cli -- allow mcp__<server>__<tool> --revoke
cargo run -p kgraf-cli -- clean <worktree> [--force]
cargo run -p kgraf-cli -- schedule add nightly --instruction task.txt --zone America/New_York --cron '0 0 3 * * *'
cargo run -p kgraf-cli -- schedule add docs-check --node <work-node-id> --instruction task.txt --zone America/New_York --cron '0 0 4 * * *'
cargo run -p kgraf-cli -- schedule list --prices prices.json
cargo run -p kgraf-cli -- schedule run-now nightly --route kimi:<model-id> --prices prices.json
```

- MCP servers are configured in `.kgraf/worker.json` (section `mcp_servers`). An
  unattended worker is denied an MCP tool until `allow` records a rule in
  `.kgraf/allow-rules.json`.
- `clean` removes a worktree whose owner was on another machine or in another PID
  namespace. It refuses while the owner or any process started in it runs; `--force` is
  needed when a run record still names it.
- `schedule` also has `remove` and `enable`. `--zone` is required and an unknown zone is
  refused; give exactly one of `--cron` (seconds first) or `--at` (RFC 3339).
- A schedule is a property of a work node. `schedule add --node <id>`
  attaches it to that node. Without `--node`, a node `schedules/<id>` is made from the
  instruction. Each run is recorded on the node: start, outcome and spend by run id. The
  galaxy shows the schedule, its last run and any disable reason on the node's label.
  `remove` detaches the schedule; the node keeps its history.
- `schedule run-now` refuses a schedule the claim table has disabled unless you pass
  `--force`, and holds the schedule's run lock while it runs, so the scheduler does not
  start the same schedule beside it and a second `run-now` is refused. `--verify` defaults to
  the first test command the project's build files declare. A run that ends blocked or
  stopped makes `orchestrate` exit non-zero, and `run-now` records it as failed.

## The desktop app

```
cargo run -p kgraf-shell --bin kgraf-app -- --project /path/to/repo
cargo run -p kgraf-shell --bin kgraf-app -- --probe
```

- `--project` opens that project and shows it, its galaxy fed live from its index (run
  `kgraf index` there first). The projects you opened before open too.
- `--probe` prints the graphics adapters and exits. `--force-software` accepts only a
  software (CPU) adapter. The window always states when it renders on the CPU.
- `--screenshot FILE` captures the window to a PNG after the first frame and
  `--screenshot-after` seconds (default 3), then quits.
- Closing the window can leave the app in the tray, where scheduled runs fire.
- **Updates.** Off by default: Settings > "Check for updates on start" lets the app read
  the GitHub Releases of `Marceux-Croix/kGraf` at start, and "Check now" always checks.
  This is the only network request the setting causes. An offered release counts on the
  bell; its menu row says "kGraf <version> is available" and carries the button that
  downloads it. Accepting twice queues one download. The download is checked against the
  release's `SHA256SUMS` and refused when the line is missing or does not match. The
  checksum file comes from the same release as the installer, so it detects a corrupt
  download and does not prove who published it; a detached signature is not built yet.
- The app builds runs from `.kgraf/run.json`: the `orchestrate` flags as JSON
  (`routes`, `prices` and the optional ones, snake case; unknown keys are refused). An old
  file that still sets `cap_run`, `cap_task` or `cap_hour` loads, prints one notice, and the
  caps do nothing (decision 0080). There is no settings screen for routes yet, so write
  this file by hand.
  Example: `{"routes": ["kimi:<model-id>"], "prices": "prices.json"}`.
- The settings file holds the compaction slider (5% to 95%, default 80%), the
  background-mode switch, the caps, the theme (device, dark or light), the color
  vision mode, reduced motion, whether the guide is open, and the open projects. Two
  windows share it: each saves under a lock and keeps the projects the other opened or
  closed. An unknown key is refused, so a settings file that a newer build changed does
  not open in an older build. A key left at its default is never written.
- **Appearance > Color vision** (Settings): standard, protanopia, deuteranopia,
  tritanopia or achromatopsia. Remaps the five semantic colors only, chosen so they
  stay distinct under a simulation of that color vision deficiency. It applies live, with
  no restart.

**The layout** uses a small set of meaning colors: violet (the accent) for progress and live, cyan for the
selection, a match and the primary action, yellow for needs input, orange on a failed
thing itself, gray for the rest.

- **Masthead:** the menu button, the kGraf mark with the branch, the project, the search
  field, the scheduler toggle with "n of m workers" (it only shows the cap; the cap is
  set in Settings), the render badge (GPU, or CPU rendering in the danger colors), and
  the bell. The bell counts everything that needs you over every open project:
  approvals, questions, held runs and failures; its menu jumps to the pinned card.
  A failure (a box run, a scheduled run, a gate or a check) is a pinned card with
  Dismiss. Dismissing clears it from the dot and the bell, and is kept in
  `.kgraf/acknowledged.json`; a new run of the same work supersedes the failure, and a
  failure of a resumed run is a new card. Scheduled failures come from the graph's
  history, so one from while the app was closed shows at launch.
- **Guide:** collapsed, a rail with Graph, Search, Providers and Settings (the only way
  into settings). The menu button opens the sidebar, which stays open until closed:
  Projects (switch, add a folder, close from the row's menu; a project that works cannot
  be closed; a close pauses its scheduler so no scheduled run starts in between, and
  while the scheduler finishes a check the row says "Closing"),
  Graph and Search, then Configuration. Each project row has a dot: yellow
  needs input (a failure counts as needs input), cyan new results since you looked,
  blinking gray working, none idle; a dot is never orange. The collapsed menu button
  takes the same order over all projects, and its tooltip names them.
  Runs in a project keep going while another is shown.
- **Center:** filter chips (status and crate; each is a search, so its result is a
  selection on the graph), then the graph on a near-black stage with its legend, minimap,
  selection title, the run scrubber (a placeholder at the live edge: going back in time
  is not built) and the view controls (legend, minimap, zoom, fit; keys
  `/`, `m`, `+`, `-`, `0`). Below: the project row, the pinned approval cards (a choice
  asks to confirm), and the composer with the tokens the ledger attributes to the run.
- **Right:** "Up next", the one text surface (Selection and Search tabs; transcripts as a
  chat, search results as rows with a static graph thumbnail), and "Live activity", the
  graph change log newest first, where a row selects its node.

**Inside the window.** The orchestrator box is always present. `/run <instruction>` starts
a paid run for the shown project through the same construction as `kgraf orchestrate`,
and says what is missing when `.kgraf/run.json` is absent or invalid. `/status`
answers from the run record. Tool approvals are answered in place (allow once, allow
always, deny). A run whose dependencies need fetching, in a project with no answer yet,
waits on a card (Allow once, Always for this project, Not now) and starts once allowed. Play, stop and your own order drive execution; the two concurrency caps
are in settings. The search field takes inline operators, and its result is a selection
on the graph, with snippets in the one text panel beside it. Work nodes show done, in
flight, blocked (with an edge to the cause), held by the cap, and colliding. Not built
yet: expanding a work node into its children, and `/compact`.

## Instructions and skills

```
cargo run -p kgraf-cli -- instructions scan
cargo run -p kgraf-cli -- instructions import ~/some/folder/with/skills
cargo run -p kgraf-cli -- instructions show
```

`instructions` gathers the instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, and
others) and `SKILL.md` skills your other coding agents already use into one inventory.
It reports conflicts and malformed files and never silently picks between two variants;
`instructions choose` resolves a conflict.

## Linux sandbox: cargo files above a worktree

A worktree lies inside your checkout (`<checkout>/.kgraf/worktrees/<name>`). Cargo run
in it reads the checkout's `Cargo.toml` and `.cargo/config.toml` above it. The Linux sandbox
refuses reads outside the worktree by default, so it grants these files one at a time.
On macOS, reads use a deny list, so this section does not apply.

- **Opt-in per command.** Gates, the verifier and the worker's shell tool ask for these
  reads. Contract checks never do: their tree is a copy under `/tmp` or another scratch
  root, and another user could put a `.cargo/config.toml` above it.
- **Bounded at the checkout.** The limit is the checkout root the caller gives, never
  derived from git. Only `Cargo.toml`, `.cargo/config.toml` and `.cargo/config` are
  granted, read-only and never executable. `~/.cargo`, `$CARGO_HOME` and your own
  read-denied paths are never entered. With no home directory known, no `.cargo` file is
  read.
- **Owner and mode trust.** A file is granted only when it and every folder from it up to
  the checkout are owned by you or root and not writable by others. Group-write is
  accepted only for your user private group: your primary group, named for you, with no
  other listed member and no other account using it as its primary group. That covers the
  umask 0002 default of Debian, Ubuntu and Fedora. If the system cannot list its accounts
  (sssd or LDAP with enumeration off), group-write is refused.
- **A refusal names its cause.** Each cargo file that exists and is not granted is
  appended to a failing gate, verification or shell command as a line such as
  `[sandbox] <checkout>/Cargo.toml is not readable in the sandbox: <checkout> is
  group-writable (mode 775) by group 100, which is not this user's private group`.
  `orchestrate` also prints each one once, as a `sandbox` line, when a run opens.

To fix a refusal, remove group or other write from the named folder or file
(`chmod g-w,o-w <path>`), or give it to your own user. Other tools that read
configuration above their directory, such as pytest, get no such rule.

## What lives where

State lives in `.kgraf/` inside the repository: the index, the ledger, run records,
worktrees, schedules and allow rules. kGraf adds `/.kgraf/` to the repository's
local exclude file (the `info/exclude` that `git rev-parse --git-path info/exclude` names,
so a linked worktree works too) and never edits your tracked `.gitignore`. If the
repository already tracks files under `.kgraf/`, it warns and names the command that
untracks them; it never changes your index. It holds the ledger, the allow rules and the
run records, so never commit it. Nothing leaves the machine except calls to the providers
and MCP servers you configure, and those calls carry code from the repository.

## Coming from Haustoria

kGraf was earlier named Haustoria. What the former name left
keeps working, and each move prints one notice:

- A project's `.haustoria/` moves to `.kgraf/` on first use. Git worktree registrations,
  attempt refs and the absolute paths in run records and configuration are repaired; the
  ledger, transcripts and traces move unchanged. A project that has both directories is
  refused until you keep one. The move is also refused, before anything changes, when
  git cannot read the repository (or is missing while worktrees exist), when an older
  process still works in a worktree, or when the old directory holds a symbolic link
  where the move would write. Anything that still names `.haustoria` after the move is
  listed in the notice.
- A `HAUSTORIA_*` variable (`HAUSTORIA_KIMI_API_KEY`, `HAUSTORIA_LOG`, ...) is still read
  when its `KGRAF_*` name is not set, with a warning naming the new variable.
- An MCP OAuth token stored under the keychain service `haustoria-mcp-oauth` moves to
  `kgraf-mcp-oauth` the first time it is read.
- The desktop app's settings and log directories move from `Haustoria` to `kGraf` on its
  first launch. On Windows, the kGraf MSI upgrades an installed Haustoria; on Debian,
  the `kgraf` package replaces `haustoria`.
- A worker transcript recorded before the rename does not resume: the worker's system
  prompt names the product, so the prefix differs and the runtime refuses, saying the
  transcript predates the rename. Finish or stop running work before you upgrade.

## Installers

Local packaging scripts build a platform-native artifact from this tree:

| Script | Artifact | Signing |
|---|---|---|
| `scripts/package-macos.sh` | universal (Apple silicon and Intel) `kGraf.app` and `.dmg` | ad-hoc only; notarization runs when a Developer ID exists |
| `scripts/package-linux.sh` | tarball and `.deb` | none |
| `scripts/package-windows.sh` | x64 `.msi` (built with `wixl`) and `.zip`, cross-built with `cargo-xwin` | none |

Each package carries `LICENSE` and `THIRD_PARTY_NOTICES.md`. The version comes from one
place, `[workspace.package].version` in the root `Cargo.toml`, and
`scripts/check-release-version.sh` fails when it does not match the highest `v*` tag's
number. The packages are not signed with a developer certificate yet, so expect Gatekeeper
and SmartScreen warnings. `scripts/release-all.sh` builds every installer from one git
tag. A `cargo-dist` workflow in `.github/workflows/release.yml` builds draft releases from a
version tag.

## Diagnostics

kGraf keeps one small, local, per-user file of basic usage counts (launches,
runs started and finished, workers spawned, the providers and models used by
name, and named feature use) and every error, each under a stable `KG-nnnn`
glossary code (`docs/ERRORS.md`). It is capped at about 1 MB, with the oldest
entries trimmed once it grows past that. **Nothing in it is ever sent
anywhere**: there is no network code in the crate that writes it at all. If
you ever need to report a problem, run `kgraf diag show` to read it, or `kgraf
diag export` to write a shareable copy, and send that however you choose.

