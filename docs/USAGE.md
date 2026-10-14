# kGraf usage guide

This guide covers installing kGraf, running work with it, and the `kgraf` command line and
`kgraf-app` desktop window. It assumes you know git and a terminal, not kGraf. It covers the
user-facing commands and flags. Internal and benchmark flags are in `--help`.

Numbers in parentheses, such as (0084), name a record in [`decisions/`](decisions).
Commands are written as `kgraf ...`. Add `-C <dir>` to run a command against another
repository.

## Contents

1. [Install and first run](#install-and-first-run)
2. [Concepts](#concepts)
3. [Command reference](#command-reference)
4. [Providers and prices](#providers-and-prices)
5. [Running work](#running-work)
6. [Permissions](#permissions)
7. [The desktop app](#the-desktop-app)
8. [Platform notes](#platform-notes)
9. [Privacy and what lives where](#privacy-and-what-lives-where)
10. [Where the detail lives](#where-the-detail-lives)

## Install and first run

kGraf builds from source. You need git 2.31 or later and [rustup](https://rustup.rs).
With rustup, the pinned Rust toolchain in `rust-toolchain.toml` (1.99.0) installs itself on
the first build. Without rustup, you need Rust 1.95 or later. On Linux, the desktop app also
needs system libraries:

```
sudo apt-get install -y --no-install-recommends \
  pkg-config libx11-dev libxi-dev libxrandr-dev libxcursor-dev \
  libxkbcommon-dev libwayland-dev libssl-dev libglib2.0-dev
```

```
git clone https://github.com/Marceux-Croix/kGraf.git
cd kGraf
cargo install --path crates/kgraf-cli     # installs `kgraf`, the command line
cargo install --path crates/kgraf-shell   # installs `kgraf-app`, the desktop window
```

Index a repository and open it:

```
cd /path/to/your/repo
kgraf index
kgraf-app --project /path/to/your/repo
```

Run a team of agents. Set each provider key in your own shell. (Vertex AI and Bedrock also
read cloud credential files; see [Providers and prices](#providers-and-prices).) Describe the
work in a text file. Write a prices file (see the same section).

```
export KGRAF_KIMI_API_KEY=...
kgraf deps allow          # let kGraf fetch the project's dependencies when a run needs them
kgraf orchestrate --route kimi:<model-id> --instruction task.txt --prices prices.json
```

The orchestrator plans the work, workers do it in separate git worktrees, and verified work
merges onto a branch named `kgraf/<run>`. Your checked-out branch never moves. The command
exits non-zero unless the run completed.

## Concepts

| Term | Meaning |
|---|---|
| **Graph** | A database of the repository's code elements (files, functions, types), their calls, their co-change history, and roadmap items. `kgraf index` builds it. |
| **Galaxy** | The desktop app's drawing of the graph. |
| **Work node** | A graph node for a unit of work. A **presumed** work node comes from a roadmap document and is marked done, partly done, or remaining. |
| **Orchestrator** | The model that turns an instruction into contracts and directs the workers. |
| **Contract** | One unit of work: an instruction, the files a worker may write, and an acceptance check. |
| **Worker** | A model that carries out one contract in its own git worktree, in a sandbox. |
| **Attempt** | One worker's result for a contract, committed to `refs/kgraf/attempts/<worker>-<n>`. A contract can have several attempts. |
| **Verify** | The command (`--verify`) that decides whether an attempt passes. |
| **Gate** | A quality command (tests, lint) that runs on the merged result. The **root gate** is the whole `--verify` command run on the merged tree. |
| **Shortfall** | A worker's report that a policy refused it something it needs, or that its instruction lacks something. Your permission mode decides what happens next. |
| **Compaction point** | The share of the context window at which a worker stops. Set it with `--point`. |
| **Hand-back** | A worker that reaches its compaction point stops and a new worker continues from its summary. |
| **Ledger** | A hash-chained record of every model call and its tokens. Cost is derived from it and a prices file. |
| **Route** | A catalog entry for one vendor, wire and address, such as `kimi`. `route:model` names a model on it. `kgraf providers` lists the routes. |
| **Model kind** | What a model returns: generative (prose and tool calls), embedding, rerank, or typed-decision. |
| **System-One** | A kind of model that answers typed questions about a state you give it, over the `/v1/systemone` wire (0042). |

## Command reference

`-C <dir>` and `--help` work on every command. A table has a Default column, or the default follows the flag in its description.

### Graph

| Command | What it does |
|---|---|
| `kgraf index` | Build the index, or bring it up to date. Flags: `--dry-run`, `--include-nested` (walk nested repositories), `--no-presume` (skip reading roadmap documents). |
| `kgraf symbol <name>` | Resolve a symbol to its nodes. |
| `kgraf slice <name> --budget N` | Print the symbol's context, sized to N estimated tokens (default 2000). |
| `kgraf query '<pattern>' --limit N` | Structural query, for example `kind=function -calls-> name=stable_id`. Limit default 50. |
| `kgraf cochange --commits N --oversize-cap N` | Compute which files change together. Defaults 2000 and 200. |
| `kgraf partners <path> --min-count N --max-commit-size N --limit N` | Files that change with a path. Defaults 1, none, 25. |
| `kgraf plan` | List presumed work nodes and their state. |
| `kgraf plan --from-roadmap --source <file>` | Read roadmap documents into presumed work nodes. `--source` is repeatable. |

Only `kgraf index` writes the index. The other commands stop and say so when it is missing
or was written by another schema version. A name that matches several nodes is refused with
the candidates listed.

An index pass skips `target`, `node_modules`, `.git` and similar output, honors the
project's `.gitignore` files and `.git/info/exclude`, and skips files over 2 MiB and files
that are not UTF-8. It prints what it skipped. It reads Rust, Python, TypeScript, TSX,
JavaScript and Markdown with kGraf's own rules. It reads Go, Java, C, C++, Ruby, C# and PHP
from each grammar's tags query, and a slice marks those members as matched by name only.

`kgraf index` also reads `ROADMAP`, `TODO`, `PLAN`, `MILESTONES`, `BACKLOG` or `TASKS`
(`.md` or `.markdown`, in the root or `docs/`) into presumed work nodes. It makes no model
call. Re-running updates the same nodes. It never duplicates or deletes them, and never
overwrites a node you changed.

### Run work

| Command | What it does |
|---|---|
| `kgraf orchestrate` | Run the orchestrator on an instruction, or resume a run. Flags below. |
| `kgraf worker` | Run one worker in its own worktree under `.kgraf/workspaces/`. It commits to an attempt ref and never merges. |
| `kgraf clean <worktree> [--force]` | Remove a kept or stranded worktree. Refused while its owner or any process started in it runs. `--force` is needed when a run record still names it. |
| `kgraf allow mcp__<server>__<tool> [--note ...] [--revoke]` | Always allow an MCP tool in this project. |
| `kgraf instructions scan \| import <folder> \| show \| choose <skill> <hash>` | Gather instruction files and skills (see [Instructions and skills](#instructions-and-skills)). |

`kgraf orchestrate` flags:

| Flag | Default | Meaning |
|---|---|---|
| `--route route:model` | required | A worker route. Repeatable. Workers rotate across routes. |
| `--planner route:model` | first `--route` | The orchestrator's own model. |
| `--model-route kind=route:model` | none | A route of another model kind. Repeatable. |
| `--instruction FILE` | | The instruction text. Conflicts with `--resume`. |
| `--resume RUN` | | Continue a run from its record. |
| `--run ID` | `run-<unix seconds>` | Id for a new run. |
| `--prices FILE` | required | The prices file. |
| `--max-items N` | 4 | Work items in flight at once. |
| `--max-agents N` | 8 | Live workers at once. |
| `--steps N` | 30 | Rounds in one block per worker. See [Run limits](#run-limits). |
| `--max-checkpoints N` | 10 | The most blocks a worker may be granted. |
| `--max-tokens N` | 16,384 | Output cap of one worker reply. |
| `--planner-max-tokens N` | 32,768 | Output cap of one orchestrator call. |
| `--window N` | 128,000 | Context window assumed for models. |
| `--point PCT` | | Compaction point, 5 to 95 percent of the window. |
| `--answer-wait SECS` | measured | How long a worker's question waits for the orchestrator's answer, 30 to 1800. |
| `--verify CMD` | `cargo test --workspace` | The acceptance command for each attempt. |
| `--gate-command CMD` | discovered | A quality gate. Repeatable. Without one, kGraf finds the project's test commands. |
| `--gate-timeout SECS` | 1800 | Longest one gate or one `--verify` run. |
| `--shell-timeout SECS` | 600 | Longest one worker shell command. |
| `--time-limit SECS` | none | The run's wall-clock budget, from when the run opens. The planner skips optional asks that would pass 30 percent of it. The last tenth (at most 120 s) is kept for verification, merge and the gate; every worker gets the rest as its deadline and stops at a round boundary before it, when its next round (the longest of its last three) would not end in time. A round still running at the deadline is cut, and its call is recorded as cancelled. The tree a stopped worker leaves is verified as it stands. When no further attempt can start, that tree is the contract's only delivery: a check that checks nothing accepts it too, with a notice, and the verification is stopped at the limit if it would run past it (the verdict names that). No new attempt starts when the time left is shorter than one round (the fastest route's last measured round, else 15 s or 5 percent of the limit); a notice says so and the run ends with what it has. Trace records: `time_limit` (actions `worker_stopped`, `tree_verified`, `attempt_refused`). |
| `--rate-limit ROUTE=RPM[/BURST][@MAX_WAIT_MS]` | none | Limit one route's call rate. A call that would wait longer than MAX_WAIT_MS (default 60000) fails. Repeatable. |
| `--routing rotation\|fitness` | `rotation` | `rotation` rotates workers across routes. `fitness` sends coupled contracts to one vendor and picks each worker's model by measured cost per satisfied task. |
| `--sessions true\|false` | true | Give every agent the session tools `ask_user_question`, `spawn_subagent`, `send_message` and `request_connection`. |
| `--reasoning [SCOPE=]LEVEL` | unset | Reasoning level. Repeatable. See [Reasoning level](#reasoning-level). |
| `--planner-reasoning LEVEL` | unset | The same as `--reasoning planner=LEVEL`. |
| `--contract-checks` | off | Experimental. Check interface contracts in the sandbox before the first worker starts. |
| `--interface-review` | off | Before a producer attempt counts as verified, one model on a different vendor's route reviews it. The finding is advisory. |
| `--no-collapse-chains` | off | Keep a plan's straight chains of contracts as the planner wrote them. By default a chain (each contract waiting on the one before) becomes one contract when its steps run on one route and one worker can carry it. Plan review and an order you chose always keep the plan as written. Also the run file key `collapse_chains` (`false`). |
| `--check-timeout SECS`, `--check-compile-timeout SECS` | 600, 600 | Limits for one contract-check test run and compile. |
| `--keep-worktrees` | off | Keep every worker's worktree and build output. |
| `--interactive` | off | Read notes and `?` status questions from stdin during the run. |
| `--fetch-deps`, `--no-fetch-deps` | ask | Answer the dependency question for this run only. |
| `--warn-rule-conflicts`, `--no-warn-rule-conflicts` | off | Warn when your verify or gate command runs a program your rules block for workers. The command still runs. |
| `--retry-gate NAME`, `--waive-gate NAME` | | With `--resume` and `--gate-note TEXT`: run a held gate again, or waive it. |
| `--corpus` | off | Plan from the repository's own documents and code. See [Start from existing documents](#start-from-existing-documents). Also `--approve`, `--accept-command`, `--correction`, `--interpret-budget` (default 40 percent of the window). |

The permission flags (`--permission-mode`, `--allow`, `--allow-path`, `--deny`, `--ask`,
`--permissions-file`, `--plan-review`, `--approve-plan`, `--script-constructs`,
`--container-sandbox`) are in [Permissions](#permissions).

`kgraf worker` flags:

| Flag | Default | Meaning |
|---|---|---|
| `--provider ROUTE`, `--model ID` | required | The route and model. |
| `--brief FILE` | required | The instruction text. |
| `--slice SYMBOL` | none | Add a symbol's graph slice to the worker's context. Repeatable. |
| `--budget N` | 12 | Rounds in one block. |
| `--max-checkpoints N`, `--max-tokens N`, `--window N`, `--point PCT` | as `orchestrate` | Same meaning. |
| `--mode complete\|stream` | `complete` | `stream` needs an adapter that streams. |
| `--events FILE` | `.kgraf/worker-<id>.jsonl` | Where the transcript goes. |
| `--resume FILE` | | Continue from a transcript. |
| `--base REV`, `--integration-branch NAME` | `HEAD`, checked-out branch | Where the worktree starts, and which branch supplies attributes. |
| `--config FILE` | `.kgraf/worker.json` | Worker configuration (MCP servers). |
| `--shell-timeout SECS`, `--reasoning` | 600, unset | Same meaning. |

### Accounting

| Command | What it does |
|---|---|
| `kgraf providers [--verified-only]` | List routes with vendor, wire, usage shape, confidence and model kinds. |
| `kgraf call --provider R --model M [--max-tokens N] [--record FILE] [prompt]` | Make one ledgered call. Max tokens default 256. The credential is never written to the record. |
| `kgraf spend [--prices FILE]` | Total the ledger. Without prices, or for a model with no price, it says unpriced. |
| `kgraf unreconciled` | List calls whose spend is not known, with the partial usage recorded for each: calls left `unreconciled` or `in_flight`, and calls shown as `cancelled` that kGraf stopped (a cancel, a deadline, a steer) before the provider reported any usage. The provider may have billed a cancelled call for what it processed before the stop, so its spend is unknown, not zero. |
| `kgraf unreconciled resolve <call-id> [--input N] [--cache-read N] [--cache-write N] [--output N] [--reasoning N] [--no-charge] [--abandoned]` | Close one listed call with the usage the provider's invoice shows. A field you omit keeps the recorded figure. `--no-charge` states the provider billed nothing. A call marked in flight needs `--abandoned`, which states its process is gone. The call is then counted at the stated usage. A call cancelled after usage was reported is already priced at that usage and is refused. There is no bulk form. |
| `kgraf ledger verify` | Walk the hash chain and report the first broken row. |
| `kgraf ledger redact <call-id> --reason TEXT` | Remove a payload captured by accident. The chain still verifies. |
| `kgraf demand` | Report whether runs offered workflow templates started more work than runs without. It claims no saving. |

### Schedules

| Command | What it does |
|---|---|
| `kgraf schedule add <id> --instruction FILE --zone ZONE (--cron EXPR \| --at TIME)` | Add a schedule. `--zone` is an IANA zone and is required. `--cron` is seconds-first (`0 0 3 * * *` is daily at 03:00). `--at` is an RFC 3339 instant. |
| | Also `--missed-policy skip\|run-once` (default `run-once`: one catch-up run for all missed occurrences) and `--node ID` (attach to an existing work node; without it a node `schedules/<id>` is made). |
| `kgraf schedule list [--prices FILE]` | Show schedules, their last run, and why one is disabled. |
| `kgraf schedule remove <id>` | Detach a schedule. The node keeps its run history. |
| `kgraf schedule enable <id>` | Clear a disable so the schedule fires again. |
| `kgraf schedule run-now <id> --route ... --prices FILE` | Run now through the same path as `orchestrate`. Takes the `orchestrate` flags for routes, `--planner`, `--model-route`, `--max-items`, `--max-agents`, `--steps`, `--max-tokens`, `--window`, `--verify`, `--shell-timeout`, `--fetch-deps`, `--no-fetch-deps`, the rule-conflict flags and `--keep-worktrees`. `--force` runs a disabled schedule. `--verify` defaults to the first test command in the project's build files. |

The desktop app fires schedules from the tray while it runs. A run that ends blocked or
stopped is recorded as failed.

### Dependencies

| Command | What it does |
|---|---|
| `kgraf deps status` | Show the stored answers, whether dependencies are ready offline, and each missing install with its command. |
| `kgraf deps allow [manager]` | Let kGraf fetch crates.io dependencies (no argument), or run a package manager's missing install in its sandbox, whenever a run needs it. |
| `kgraf deps refuse [manager]` | Never fetch or install. A run that needs the cargo fetch stops before it starts. A run that needs another install goes on without it and says so. |
| `kgraf deps reset [manager]` | Forget the answer. The next run that needs it asks. |
| `kgraf deps fetch` | Run the cargo fetch now. |
| `kgraf deps install [manager]` | Run the missing installs now, once. |

Managers: `npm` (also pnpm, Yarn, Bun), `python`, `go`, `maven`, `gradle`, `dotnet`,
`composer`, `bundler`.

### Maintenance and sandbox

| Command | What it does |
|---|---|
| `kgraf diag show` | Print the diagnostics file and its path. |
| `kgraf diag export [--out FILE]` | Write a scrubbed copy you can share. Default `kgraf-diag-export-<ts>.jsonl` in the current directory. |
| `kgraf sandbox setup [--slots N]` | Windows only. Create the sandbox users. Default 4 slots. |
| `kgraf sandbox status` | Show what setup created. |
| `kgraf sandbox remove` | Windows only. Undo setup. |

### The desktop app

| Flag | Meaning |
|---|---|
| `--project DIR` | Open that project. Run `kgraf index` there first. Projects you opened before open too. |
| `--config FILE` | Settings file. Default is the platform config directory. |
| `--probe` | Print the graphics adapters and exit. |
| `--force-software` | Accept only a CPU adapter. |
| `--measure SECONDS` | Measure frame timing after the first frame, then print a report and exit. |
| `--continuous` | Repaint every frame instead of only on change. |
| `--no-vsync` | Present without vsync. |
| `--no-tray` | Run without a tray icon. Background mode is then unavailable. |
| `--screenshot FILE`, `--screenshot-after SECONDS` | Save the window to a PNG after the first frame plus this delay (default 3), then quit. |

## Providers and prices

Each route reads its key from its own environment variable. The variable is
`KGRAF_<ROUTE>_API_KEY`, with the route id in upper case and `-` as `_`. Routes that share
a vendor key share a variable: `kimi` and `kimi-anthropic` read `KGRAF_KIMI_API_KEY`, and
`kimi-code` reads `KGRAF_KIMI_CODE_API_KEY`. Local routes (`ollama`, `llama-cpp`,
`systemone`, `tei`) take no key. Set keys in your own shell. Two routes also read cloud credential files: Vertex AI and Bedrock (below).

[MODEL-SUPPORT.md](MODEL-SUPPORT.md) lists which routes and model kinds have run against
the live service (VERIFIED) and which are built but untested. Read it before you choose a
route.

**Prices file.** A JSON object from model name to five rates, in micro-dollars per million
tokens (3,000,000 is $3.00):

```json
{
  "<model-id>": {
    "uncached_input": 3000000,
    "cache_read": 300000,
    "cache_write": 3750000,
    "output": 15000000,
    "reasoning": 15000000
  }
}
```

The numbers above are examples, not real rates. `orchestrate` and `schedule run-now`
require the file. `spend` and `schedule list` accept it. kGraf uses it only to show cost.
kGraf sets no spend limit (0080): set one in your provider's console. A model with no entry
still runs, and its spend is shown as unpriced, never as zero. A key written
`<model>@<route>` applies to calls on that route and wins over the model's own entry. Azure
routes are priced only under that form, because a deployment name is your own choice.

**Moving a local route.** Set `KGRAF_<ROUTE>_BASE_URL` to an `http://` or `https://` URL.
Only routes that take no key can be moved this way. Defaults: `ollama`
`http://localhost:11434/v1`, `llama-cpp` and `tei` `http://localhost:8080/v1`, `systemone`
`http://localhost:8000`. Every local route in a run must accept a connection when the run
starts, or the run fails before it builds anything.

### AWS Bedrock

Route `bedrock` (Converse API). Listed as unverified. Credentials, in order of precedence:

```
export AWS_BEARER_TOKEN_BEDROCK=...       # a Bedrock API key
# or access keys
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_SESSION_TOKEN=...              # only for temporary keys
export AWS_REGION=us-east-1               # or AWS_DEFAULT_REGION
```

kGraf also reads a profile with static keys from `~/.aws/credentials` (or the file `AWS_SHARED_CREDENTIALS_FILE` names) (`AWS_PROFILE`). A profile
that needs SSO, role assumption, a credential process or web identity is refused. Export
temporary keys instead. The model id is the Bedrock model id or inference profile id.

### Google: Gemini and Vertex AI

| Route | Reaches | Credential |
|---|---|---|
| `gemini` | Gemini through `generateContent` | `KGRAF_GOOGLE_API_KEY` |
| `vertex-gemini` | Gemini on Vertex AI | Vertex settings |
| `vertex-anthropic` | Anthropic models on Vertex AI | Vertex settings |

All three are listed as unverified. The `google` route is Gemini's OpenAI-compatible layer,
which ignores parameters it does not support. Use `gemini` for the thinking control.

```
export KGRAF_VERTEX_CREDENTIALS=adc       # or metadata, gcloud, or a service-account key file path;
                                          # if unset, GOOGLE_APPLICATION_CREDENTIALS is used
export KGRAF_VERTEX_PROJECT=my-project    # optional with a key file
export KGRAF_VERTEX_REGION=us-east5       # or global; there is no default
```

`adc` reads the key file `GOOGLE_APPLICATION_CREDENTIALS` names, else the gcloud user
credentials file. `metadata` asks the metadata server. `gcloud` runs
`gcloud auth print-access-token`. Where a `KGRAF_VERTEX_*` variable is unset, kGraf reads
`GOOGLE_CLOUD_PROJECT` (or `GCLOUD_PROJECT`) and `GOOGLE_CLOUD_LOCATION` (or
`GOOGLE_CLOUD_REGION`).

### Azure OpenAI

Routes `azure-openai` (Chat Completions) and `azure-openai-responses`. Both are unverified.

```
export KGRAF_AZURE_OPENAI_API_KEY=...
export KGRAF_AZURE_OPENAI_RESOURCE=my-resource     # the name alone
export KGRAF_AZURE_OPENAI_API_VERSION=v1           # the Responses route takes v1 only
export KGRAF_AZURE_OPENAI_DEPLOYMENTS=my-chat=gpt-4o-2024-08-06
kgraf call --provider azure-openai --model my-chat
```

The model you pass is the deployment name. `KGRAF_AZURE_OPENAI_DEPLOYMENTS` maps each
deployment to the exact model string the service returns, dated suffix included. A reply
that names another model stops the call and leaves its spend unreconciled. Price a
deployment with a `<deployment>@azure-openai` entry.

### Other model kinds

`--route` and `--planner` are always generative. Declare any other kind with
`--model-route kind=route:model` (repeatable), or `model_routes` in `.kgraf/run.json`.

| Kind | Tool | Served by |
|---|---|---|
| `embedding` | `embed` | `openai`, `google`, `llama-cpp`, `ollama`, `tei`, Azure routes |
| `rerank` | `rerank` | `together`, `llama-cpp`, `tei` |
| `typed-decision` | `decide` | `systemone` |

```
kgraf orchestrate --route kimi:<model> --prices prices.json --instruction task.txt \
  --model-route embedding=openai:text-embedding-3-small \
  --model-route typed-decision=systemone:jev-latest
```

A model route runs no worker. The orchestrator is told which routes exist and may call
their tools while it plans. Every worker gets the tool too. Nothing calls one by default.
Every call is a ledger row. A route that does not serve the kind you name is refused, never
rerouted.

To use a System-One model, start a server that answers `POST /v1/systemone` at
`http://localhost:8000`, then declare the route. The server's default model alias is
`jev-latest`. The route sends no credential. A typed answer outside its declared space is
refused. A typed answer is not a verified answer.

## Running work

### The sequence

1. **Setup.** The run indexes the working tree, checks the dependencies (see below) and
   checks that each local route answers.
2. **Plan.** The orchestrator splits the instruction into contracts. A malformed answer is
   asked for again, at most twice. A straight chain of contracts, each waiting on the one
   before, becomes one contract when its steps run on one route and one worker can carry it
   (a chain gains no parallelism, and each split costs an interface and a handshake). The run
   notices say so, and the trace holds a `chain_collapsed` record. A plan under `--plan-review`
   stays as written, as does a plan with an order you chose; with several routes and steps
   that are not pinned to one route, chains stay split.
3. **Dispatch.** Workers run in parallel, each in its own worktree and sandbox, each fed
   from graph slices.
4. **Verify.** Each attempt runs the `--verify` command. An attempt that changed none of
   its contract's declared files is not verified, even if the command passes.
5. **Merge.** Verified work merges onto `kgraf/<run>`. The gates run, and `--verify` runs
   again as the root gate on the merged result. The run completes only when the root gate
   passes.

When `--verify` is a whole-suite runner kGraf recognizes (`cargo test --workspace`,
`pytest`, `go test ./...`, `npm test`, `mvn test`, `gradle test` and others), each contract is
first checked on the tests of its own files only. A check that fails only because other
contracts have not merged yet is deferred to the root gate (0018).

### Resume and stop

Press Ctrl-C, or send SIGTERM, to stop `orchestrate` or `schedule run-now`. Nothing new
starts, every worker and model call in flight is cancelled, and the ledger settles each call.
A worker's call stopped this way settles as cancelled, with the usage its stream reported
before the stop. One stopped before any usage arrived is charged at its worst case, since the
provider may have billed it, and `kgraf unreconciled` lists it as `cancelled` so you can resolve
it with the provider's figure. A call that ended for another reason and may have billed also
appears in `kgraf unreconciled`. The report prints with status
`Stopped`. Each worktree the run left is named with how to continue or remove it. The exit
code is 128 plus the signal number (130 for Ctrl-C, 143 for SIGTERM). Stopping can take up to
30 seconds. A second interrupt exits at once.

Continue a stopped or killed run with `kgraf orchestrate --resume <run-id> --route ... --prices ...`.
A resume does not repeat a paid call. A held or failed gate is run again, or waived, with
`--resume`, `--retry-gate` or `--waive-gate`, and `--gate-note`.

### Run limits

Six limits are global settings. Each has a slider in the app under Settings > Runs and a
key in the user-level `settings.toml`.

| Limit | `settings.toml` key | Range | Flag | Default |
|---|---|---|---|---|
| Rounds in one block, per worker | `rounds_per_worker` | 4 to 200 | `--steps` (`orchestrate`, `schedule run-now`), `--budget` (`worker`) | 30 orchestrated, 12 for `kgraf worker` |
| Most blocks a worker may be granted | `max_checkpoints` | 1 to 50 | `--max-checkpoints` | 10 |
| Worker reply output cap (tokens) | `worker_max_tokens` | 1,024 to 131,072 | `--max-tokens` | 16,384 |
| Orchestrator call output cap (tokens) | `planner_max_tokens` | 4,096 to 131,072 | `--planner-max-tokens` | 32,768 |
| Context window assumed (tokens) | `context_window` | 8,192 to 1,000,000 | `--window` | 128,000 |
| Answer wait (seconds) | `answer_wait_secs` | 30 to 1,800 | `--answer-wait` | measured per call |

A **block** is a set of rounds. Near the end of a block, kGraf measures what the worker did:
files changed, its own test or build runs improving, and no sloppy pattern (the same error
repeated, failed patches piling up, the same call with the same result) (0096). A worker
that makes progress gets another block, up to `max_checkpoints`. Two blocks in a row without
progress end the attempt as stuck. kGraf never reads what the model says about its own
progress. With `max_checkpoints` at 1, the round limit is fixed.

A block of 9 rounds or more also gets one early probe, a third of the way in (round 10 of 30).
It takes the same measure. A worker that has changed no file and improved no check, or that
repeats a failure, is told once to write its best attempt now or report what blocks it. The
probe never ends the attempt.

When an attempt ends before verification (out of rounds, stuck, or stalled), the next attempt's
brief says what stopped it and what it tried: the files it wrote and the commands it ran, each
with its exit status and, for a failure, its error line. Repeated commands are listed once with
a count. The list is built from the attempt's record without a model call and is capped at
about 1,500 tokens, oldest lines dropped first. Only the previous attempt's list is carried.

A single worker uses at most 100,000 tokens of the window (0057). A reply cap above what the
model supports is lowered for the run, and the run says so on a `clamped` line.

**Precedence**, highest first:

| Order | Source | Applies to |
|---|---|---|
| 1 | Command-line flag | the command |
| 2 | Key in `.kgraf/run.json` | runs the desktop app starts; counts as a flag for that project |
| 3 | User setting in `settings.toml` | every run |
| 4 | Built-in default | every run |

The command line never reads `run.json`. A setting outside its range, or not a whole number,
stops the command and names the key. Each command prints the effective values, and where each
came from, as `limits` lines on stderr. `KGRAF_SETTINGS_PATH` points the command line at a
different settings file.

### Reasoning level

`--reasoning` sets how much a model reasons on each call. Repeat it for more scopes.

| Form | Applies to |
|---|---|
| `--reasoning LEVEL` | every call |
| `--reasoning SCOPE=LEVEL` | a group (`planner`, `checks`, `worker`) or one call kind (`decompose`, `worker_round`, and others the ledger records in `call_kind`) |

Levels: `unset` (send no field; the model's default applies; this is the default), `off`,
`low`, `medium`, `high`. The most specific scope wins. `off` is sent only where a wire form
is verified to turn reasoning off, and every answer is checked. A graded level measured to
have no effect on a route is refused before any call. A graded level never measured there is
sent, and the run says so with `UNVERIFIED`. See "Reasoning level by route" in
[MODEL-SUPPORT.md](MODEL-SUPPORT.md). The same settings live in `run.json` as a `reasoning`
object and in `settings.toml` as a `[reasoning]` table.

### Dependencies

Workers build and test with the network off (0084). A run therefore needs the project's
dependencies on disk before it starts.

- **Ask once per project.** `kgraf deps allow` lets kGraf run `cargo fetch` for crates.io
  dependencies when a run needs them. The answer is stored in `.kgraf/dependency-fetch.json`.
- **Other languages.** Install them yourself (`npm ci`, `pip install -r requirements.txt` in
  a `.venv`, `go mod download`, and so on), or run `kgraf deps allow npm` (or `python`,
  `go`, and so on) to let kGraf run that install in its sandbox. `kgraf deps status` shows
  each command.
- **With no answer**, `orchestrate` at a terminal asks. The app shows a card. An unattended
  run stops before it starts and says how to answer.
- **For one run only**, use `--fetch-deps` or `--no-fetch-deps`. They store nothing.
- **Workers never download.** A command that downloads packages is refused in every
  permission mode, Bypass included. Only crates.io is fetched, and a git or other registry
  dependency stops the fetch.

Workers cannot merge changes to `.cargo`, `rust-toolchain`, and other build-tool or
version-manager configuration. Make those by hand.

### Worker worktrees

kGraf deletes a worker's worktree, with its build output, once nothing needs it. To keep
them, pass `--keep-worktrees` to `orchestrate` or `schedule run-now`. In the app, turn off
Settings > Runs > "Delete worker worktrees after use". `kgraf clean <worktree>` removes a
kept one. A worktree that a live worker, a stopped run or a pending merge needs is never
deleted. A worktree that cannot be deleted stays, and the report names it.

### Start from existing documents

`kgraf orchestrate --corpus --route ... --prices ...` reads the repository's documents and
code, prints the plan it read with a plan id, and stops. Nothing is created in the
repository. Add `--correction "..."` to read again with a correction. `--approve <plan-id>`
runs that stored plan with no new model call. A proposed validation command becomes a gate
only when you name it with `--accept-command`. In an empty directory, also give
`--instruction`.

### Instructions and skills

`kgraf instructions scan` gathers `CLAUDE.md`, `AGENTS.md`, `GEMINI.md` and other
instruction files, and `SKILL.md` skills, that your other coding agents use, into
`.kgraf/inventory.json`. `import <folder>` adds a folder. `show` prints layers, skills,
conflicts and problems. kGraf never silently picks between two variants of a skill: resolve
a conflict with `choose`.

Runs read the inventory once, at start. Run `scan` again to pick up changes. Each worker's
prompt lists the skills by name and description, and the worker gets a `skill` tool to
activate one. A skill with `disable-model-invocation: true` is not listed. A skill's
`allowed-tools` list restricts the worker to those tools once it activates the skill. Entries
name kGraf's tools (`read_file`, `list_directory`, `search_files`, `apply_patch`,
`replace_in_file`, `shell`, `graph_slice`, `skill`, `mcp__<server>__<tool>`). A name from
another agent, such as `Bash`, matches nothing here.

### MCP tools

Configure MCP servers in `.kgraf/worker.json`, section `mcp_servers`. An unattended worker is
denied an MCP tool until you record a rule with `kgraf allow mcp__<server>__<tool>`. Remove
it with `--revoke`. The rules live in `.kgraf/allow-rules.json`.

## Permissions

One mode decides who answers a permission request that the base policy does not settle. It
applies to a worker's approval ask and to a shortfall report alike (0094). Choose it in the
orchestrator box in the app, or with `--permission-mode` on the command line. The default is
Manual.

| Mode | `--permission-mode` | Who answers |
|---|---|---|
| Manual | `manual` | You answer every request. |
| Accept edits | `accept-edits` | File edits are granted. Commands, network and MCP tools ask you. |
| Auto | `auto` | What your allow set names is granted. Anything else asks you. |
| Orchestrator decides | `orchestrator-decides` | The orchestrator grants or refuses each request against the contract. It never asks you. |
| Bypass | `bypass` | Everything is granted without asking. |

**Bypass offers no protection against prompt injection or unintended actions.** Workers may
also write the repository's git directory and kGraf's `run.json` and `worker.json`. kGraf
checks the repository after such writes and puts a damaging write back. Use Bypass only in a
disposable environment. `--container-sandbox` selects Bypass for a run inside a disposable
container and also requires `KGRAF_CONTAINER_SANDBOX=1`. `disable_bypass = true` in
`permissions.toml` refuses the mode from every source. Bypass is refused when kGraf runs as
root, unless it was started in a container that says so.

**Rules that hold in every mode.** A deny rule blocks matching calls, Bypass included. An ask
rule asks. Deleting a system path is refused. A run nobody attends denies what would ask.

**Files a command writes outside the contract's files.** A worker's file tools are refused a
path its contract does not list, and the mode decides whether to add it. A shell command is not
stopped per path, so a file it writes outside the list is decided when the worker finishes, the
same way: a mode that would grant the path (Bypass, Orchestrator decides, Accept edits, or Auto
with the path in `--allow-path`) adds it to the contract, records the grant and says so in a
notice. A path another contract lists is never added, in any mode. Otherwise the attempt fails
and its reason says why. If the next attempt fails on the same paths, the contract is held at
once instead of retried.

**Processes a worker leaves running** end when its attempt ends, with a notice. In a task
container (the Terminal-Bench bridge) the accepted attempt's processes stay, since the task
judges the container, and the other attempts' processes end.

**Where to set them.**

| Setting | Flag | `permissions.toml` key |
|---|---|---|
| Deny rule `TOOL` or `TOOL(PATTERN)`, for example `shell(curl*)` | `--deny` | `deny = [...]` |
| Ask rule, same form | `--ask` | `ask = [...]` |
| Programs Auto may grant | `--allow PROGRAM` | `[allow] programs` |
| Paths Auto may widen a write scope to | `--allow-path PATH` | `[allow] paths` |
| Tools Auto may grant | | `[allow] tools` |
| Show the plan and wait for your yes | `--plan-review`, `--approve-plan` | `plan_review` |
| Shell constructs a mode may grant inside `sh -c` | `--script-constructs LIST` | `script_constructs` |

The file is `permissions.toml` in kGraf's per-user configuration directory.
`KGRAF_PERMISSIONS_PATH` or `--permissions-file` points elsewhere. The file is refused if it
lies inside the project. The app's Settings > Permissions edits it.

`--script-constructs` takes a comma-separated list of `variables`, `subshells`, `background`,
`input-redirects`, `escapes` and `outside-redirects`, or `none`. All are off by default. With
one on, a script that uses it is granted when every program kGraf can see in it is granted.
kGraf cannot see what a variable or substitution runs.

**Build and test commands** run without approval, inside the sandbox with the network off:
`cargo build`, `check`, `test`, `clippy` and `fmt --check`, and the common build and test
commands of npm, pytest, go, Maven, Gradle and dotnet, each under argument rules (0084).
The run's own `--verify` and `--gate-command` commands run as written. Change the list with
`build_commands` in `.kgraf/worker.json`.

**Shortfalls.** A worker whose call a policy refused is told to report it and stop (0093). A
report is judged by what was actually refused, never by its wording. Your mode then decides:
you answer, the mode grants, or the orchestrator decides. A grant your deny or ask rule would
meet again is never made. A contract is re-dispatched at most twice for shortfalls. Every
grant, hold and answer is in the run's trace. In a run nobody attends, under Bypass, a report
that the instruction falls short because of the environment (a missing module, no network, a
command the worker believed was not allowed) is answered with the facts, and the worker keeps
working; the trace names the fact. A real gap in the instruction is amended once, then held.

**What a worker is told.** The shell's description and the worker's system prompt match the
run. In Bypass they say any command runs without approval. When commands run in a task
container (the Terminal-Bench bridge), they say so, and say that kGraf does not cut off the
network there and that installing packages is allowed. A worker on this machine outside
Bypass is told what it was before.

## The desktop app

The app shows the project's galaxy, fed live from its index. Run `kgraf index` first.

**Layout.** A left rail and sidebar hold Projects, Graph, Search and Settings. The center is
the graph, with filter chips, a legend, a minimap and zoom controls (keys `/`, `m`, `+`, `-`,
`0`). Below it are the pinned approval cards and the orchestrator box. The right side holds
"Up next" (the selection, search results and transcripts) and "Live activity" (the graph's
change log, newest first). The bell counts everything that needs you across open projects:
approvals, questions, held runs and failures. A failure is a pinned card with Dismiss.
Runs in a project keep going while you view another.

**The orchestrator box.** Plain text is a note for the running run. In a window with no run,
use `/run <instruction>` to start a paid run through the same path as `kgraf orchestrate`. It
needs `.kgraf/run.json`. `/status` asks where the run stands. `/compact` compacts the
orchestrator, and `/compact selected` or `/compact <worker>` targets a worker. `/help` lists
them. Tool approvals are answered in place: allow once, allow always, deny.

**Runs read `.kgraf/run.json`.** It holds the `orchestrate` flags as JSON, with snake_case
keys. Unknown keys are refused. `worktrees` is refused too: the Settings switch decides it.
`routes` and `prices` are required.

```json
{
  "routes": ["kimi:<model-id>"],
  "prices": "prices.json",
  "model_routes": ["embedding=openai:text-embedding-3-small"]
}
```

**Settings** has five tabs. A change applies at once. There is no Save button.

| Tab | What it holds |
|---|---|
| General | Background mode (the app stays in the tray, where scheduled runs fire). Update checks. |
| Runs | Parallelism, the compaction point, the six run limits, deleting worker worktrees after use. |
| Permissions | The rule editor, the allow set, shell-script switches, Allow Bypass mode, the rule-conflict warning. |
| Appearance | Theme, color vision mode, reduced motion, the GPU preference. |
| Providers | The active project's routes, read-only. |

The **compaction point** is the share of the window at which a worker hands back. The slider
runs from 5 to 95 percent and defaults to 80. A `point` key in `run.json` outranks it. The
command line takes `--point`. The point is judged on the token counts the provider reports.
Before the first report, kGraf only estimates, and the estimate runs high. So a worker whose
brief is estimated past the point still runs its first round, and its transcript records both
numbers. kGraf refuses a brief only when the estimate reaches the whole window.

**Updates.** Off by default. Settings > General > "Check for updates on start" lets the app
read the GitHub Releases of `Marceux-Croix/kGraf` at start. "Check now" always checks. A
download is checked against the release's `SHA256SUMS`. That detects a corrupt download. It
does not prove who published it.

**Rendering.** The app says when it renders on the CPU. On Linux the tray uses the
StatusNotifierItem D-Bus service. KDE Plasma has a host for it. GNOME needs the AppIndicator
extension.

## Platform notes

| | macOS | Linux | Windows |
|---|---|---|---|
| Built | Yes (universal `.dmg` by script) | Yes (tarball and `.deb` by script) | Yes (x64 `.msi` and `.zip` by script, cross-built) |
| Tested | Development platform | Tested on Ubuntu 24.04 in CI and in containers ([guide](LINUX-TESTING.md)) | Tested on a hosted Windows Server 2025 runner. Not yet on Windows 11 or ARM64 ([guide](WINDOWS-TESTING.md)) |
| Sandbox setup | None | None. Needs unprivileged user namespaces | Once per machine, with administrator rights |
| Network off for worker commands | Yes | Yes | Yes, after setup. Gap: DNS (below) |
| Reads outside the worktree | Only credential and tool-config paths are denied | Denied by default | Not confined for dependency installs |

The packages are not signed with a developer certificate, so expect Gatekeeper and
SmartScreen warnings. `scripts/package-macos.sh`, `scripts/package-linux.sh` and
`scripts/package-windows.sh` build them. `scripts/release-all.sh` builds all three from one
tag.

**Linux.** Ubuntu 23.10 and later block user namespaces for a program with no AppArmor
profile that grants them. The `.deb` installs `/etc/apparmor.d/kgraf` and loads it. Without
user namespaces, kGraf refuses the sandboxed installs and hidden folders, and says so. For a
tarball install, from the unpacked folder:

```
sudo install -m 644 share/apparmor/kgraf /etc/apparmor.d/kgraf
sudo apparmor_parser -r -W /etc/apparmor.d/kgraf
```

**Windows.** kGraf needs Git for Windows (git and Git Bash). It is not bundled. At start,
kGraf looks for Git Bash. If it is missing and `winget` is available, kGraf asks once
whether to install Git for Windows for your user. If you decline, or `winget` is missing,
kGraf stops and shows the download link. `KGRAF_GIT_BASH_PATH` names a Git Bash in a custom
place.

Windows runs sandboxed commands as dedicated local users. Set them up once, from a normal
shell (the command asks for administrator rights through the UAC prompt):

```
kgraf sandbox setup     # create the users, firewall rules and filters
kgraf sandbox status    # show what exists; needs no administrator rights
kgraf sandbox remove    # undo everything setup made
```

`--slots N` (default 4) sets how many worker slots get users of their own, so commands in
different worktrees cannot write each other's files. Run `setup` again with a larger N if
a command reports that no slot is free.

Known gap: Windows Server 2025 refuses the filter that confines DNS lookups. Setup says
so. Windows dependency installs then run with the network open, and the install and its
scripts can read any file you can. The install question says so. Detail:
[WINDOWS-TESTING.md](WINDOWS-TESTING.md).

## Privacy and what lives where

kGraf is local and single-user. It has no account, no hosted mode and no telemetry. This
table is the complete list of network paths in the program:

| Destination | When | Carries |
|---|---|---|
| Model providers you configure | Every model call | Prompts, which include code from your repository |
| MCP servers you configure, and their OAuth endpoints | When a worker uses their tools | Tool calls and results |
| A package registry | Only for a project where you ran `kgraf deps allow` or passed `--fetch-deps`. crates.io for cargo. The registries of the package manager you allowed for installs. | Package requests |
| GitHub Releases of `Marceux-Croix/kGraf` | Only if you turn on "Check for updates on start" or press "Check now" | A version query, and the installer if you accept it |
| `winget` | Windows only, only if you agree to install Git for Windows | `winget`'s own downloads |
| Anywhere, from a dependency install | Windows only, when you allow installs. The network is open, and DNS is not confined on Windows Server 2025 | The install and its scripts, which can also read any file you can (see [Platform notes](#platform-notes)) |

Local routes (Ollama, llama.cpp, TEI, System-One) talk to your own machine or the address
you set. kGraf's own git never uses a network transport. Workers' commands run with the
network off.

**`.kgraf/`** in each repository holds the state of that project: `graph.db` (the index),
`ledger.db`, `runs/` (run records and traces), `worktrees/` and `workspaces/`,
`worker.json`, `run.json`, `allow-rules.json`, `inventory.json`, `dependency-fetch.json`,
`schedule.db`, `package-stores/` and `acknowledged.json`. kGraf adds `/.kgraf/` to the repository's
`.git/info/exclude` and never edits your tracked `.gitignore`. It holds the ledger and run
records, so do not commit it. An old `.haustoria/` folder is moved to `.kgraf/` on first use.

**Diagnostics.** kGraf keeps one local file, `diagnostics.jsonl`, in its per-user data
directory. `kgraf diag show` prints its path and contents. It holds basic usage counts
(launches, runs, the providers and models used by name, feature use) and every error, each
under a `KG-nnnn` code ([ERRORS.md](ERRORS.md)). It is capped near 1 MB, and the oldest
entries are trimmed. **kGraf never sends this file.** To report a problem, run
`kgraf diag export` and send the copy however you choose. To move the file, set
`KGRAF_DIAG_PATH` to a path. To turn diagnostics off, set `KGRAF_DIAG_DISABLE` to any value.
An empty `KGRAF_DIAG_PATH` also turns them off.

**Logging.** `KGRAF_LOG` sets the log filter. The default is `error`. Log targets that can
carry a secret stay locked whatever you set.

**kGraf's own git.** kGraf runs git on your machine to create worktrees, commit and merge. It
ignores repository hooks, filters, merge drivers, credential helpers and `GIT_*` variables
in the environment, apart from your commit identity. It sets `core.autocrlf=false` for its
own git and for workers. Your own git is unchanged.

## Where the detail lives

| Topic | Where |
|---|---|
| Which routes and model kinds are verified | [MODEL-SUPPORT.md](MODEL-SUPPORT.md) |
| Error codes (`KG-nnnn`) | [ERRORS.md](ERRORS.md) |
| Why kGraf works as it does | [`decisions/`](decisions) |
| System design | [ARCHITECTURE.md](../ARCHITECTURE.md) |
