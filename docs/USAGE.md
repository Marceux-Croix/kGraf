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
| `kgraf land --run RUN [--into BRANCH] [--commit SHA] [--untested] [--recover]` | Land a run onto your branch as one merge commit. See [Landing](#landing). |
| `kgraf mirror rebuild RUN` | Write the run's plan mirror in the graph again from its run record. See [The plan in the graph](#the-plan-in-the-graph). |
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
| `--change FILE` | | With `--resume`: reopen the run with a change. See [Change a run](#change-a-run). |
| `--edit FILE` | | With `--resume`: reopen the run with its instruction as you edited it. See [Change a run](#change-a-run). |
| `--from-run RUN` | | With `--instruction`: start a new run from a copy of RUN's plan. See [Change a run](#change-a-run). |
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
| `--contract-checks` | off | Experimental. Check interface contracts in the sandbox before the first worker starts. The planner is asked for an interface only where the producer writes a source file, at most twice. A check that still fails after the plan's one re-plan is dropped with a notice, and the contracts run. |
| `--interface-review` | off | Before a producer attempt counts as verified, one model on a different vendor's route reviews it. The finding is advisory. |
| `--no-collapse-chains` | off | Keep a plan's straight chains of contracts as the planner wrote them. By default a chain (each contract waiting on the one before) becomes one contract when its steps run on one route and one worker can carry it; a link stays split when its first step writes source code an interface can check and both steps are large (each at least a quarter of the route's plan target), so both can start at once against the first step's stub. Plan review and an order you chose always keep the plan as written. Also the run file key `collapse_chains` (`false`). The run record does not keep it: give it again with `--resume`. |
| `--early-nonparty` | off | With `--contract-checks`, start the contracts whose gate has nothing to do on the loop before a contract whose gate still has a stub to land or a handshake to make. Those two steps run on the loop, and the run starts one contract at a time, so a ready contract waits behind them. A contract an interface binds whose checks still run, or whose checks passed and whose stub is landed, keeps its place. The order you chose and every contract's own checks are unchanged. Routes are handed out in the order of dispatch, so a contract can get another route than without the flag. Also the run file key `early_nonparty` (`true`). The run record does not keep it: give it again with `--resume`. |
| `--plan auto\|full` | `full` | `auto` sizes the instruction first: a small request runs as one contract with no planning call, a large one is planned, and one in between gets one short triage call. See [Small requests](#small-requests). Also the run file key `plan` and the `plan_mode` user setting. |
| `--early-start`, `--no-early-start` | off | With `--plan auto`, start the one worker beside the triage call when it can start at once. Also the run file key `early_start` and the `early_start` user setting. |
| `--own-checks`, `--no-own-checks` | off | A plan's contract may carry a check command of its own, run only when `--verify` tests nothing for it. See [A contract's own check](#a-contracts-own-check). Also the run file key `own_checks` and the `own_checks` user setting. |
| `--zero-test-plans`, `--no-zero-test-plans` | off | Apply the zero-test rule of [Small requests](#small-requests) to every contract. Also the run file key `zero_test_plans` and the `zero_test_plans` user setting. |
| `--concurrent-planner`, `--no-concurrent-planner` | off | Run the planner calls a contract waits on before it starts (an interface's handshake, its stub land, its consumers' tests, a re-grounding) beside the run: other contracts keep verifying, merging and starting meanwhile, and the contract that needs the answer waits for it. Off, the run waits for each such call, as before. The first plan is made the same way either way. Also the run file key `concurrent_planner`, the `concurrent_planner` user setting (Settings > General > "Keep work moving during planner calls" in the app). |
| `--cache-layout`, `--no-cache-layout` | off | A change to a finished run sends its plan block in contract id order and marks it for the provider's prompt cache where the route takes marks and a read can still come, with the run's cache key. See [Change a run](#change-a-run). Also the run file key `cache_layout` and the `cache_layout` user setting (Settings > General). |
| `--check-timeout SECS`, `--check-compile-timeout SECS` | 600, 600 | Limits for one contract-check test run and compile. |
| `--keep-worktrees` | off | Keep every worker's worktree and build output. |
| `--interactive` | off | Read notes and `?` status questions from stdin during the run, and answer a change's ask with y or n. A note is acknowledged as soon as it is saved, and a status question answered, even while the run waits on a planner call. |
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
   notices say so, and the trace holds a `chain_collapsed` record. A link stays split when its
   first step writes source code an interface can check and both steps are large (each step's
   own instruction and code at least a quarter of the route's plan target): the first step's
   interface stub lets both start at once. A trace notice names the link and the reason, and
   the rest of the chain on either side is merged as usual. A plan under `--plan-review`
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

### Small requests

With `--plan auto` (Settings > General > "Run small requests without a plan" in the app), kGraf
sizes the instruction by a rule before it asks the planner, with no model call (decision 0100):

| Size | When |
|---|---|
| small | at most 400 characters, one sentence or one list item, one or two named files that exist or sit in an existing directory, no roadmap phase or decision named, and no other contract holding a lease on a named file |
| large | over 1,500 characters, three or more sentences or list items, four or more named files across two or more top-level directories, or a roadmap section named |
| neither | one `triage` call: reasoning off, at most 64 output tokens, never asked again |

A small request, or one the triage call answers "one", is one contract: the instruction
verbatim, writing the named files and the other files of their package (at most 64; past that,
the named files alone, and a write outside them goes through the permission mode's grant).
There is no `decompose` or `interfaces` call. Its check is `--verify` narrowed to the tests of
the files it writes. A narrowed `cargo test` that selects no recognized test (a file under
`tests/`, a `#[test]`, `#[tokio::test]`, `#[rstest]`, `proptest!` or a doctest), with no
dependent package's check beside it, leaves the contract unverified (`scoped_check_vacuous`)
until the root gate passes on a tip that holds its merge; a notice says when the mark is
cleared. This applies to a small request's contract only, unless `--zero-test-plans` (Settings >
General > "Hold planned work that no test checks") applies it to every contract; then every pass
of the root gate, not only a background one, clears the marks of the contracts merged on its tip. A request that names no file is never small: its one contract would have no write
scope, so it is asked about (or planned in full when it is large). A name with no directory
(`config.rs`) counts as a file only when exactly one indexed file has that name; a dotted word
(`Node.js`, `self.plan`) is not a file and makes the request ask. The triage call has a deadline:
twice the planner route's measured (or default) p90 for it, at most 30 s and the run's time
left; past it the call is cut, settles cancelled, and the request is planned in full with a
notice. A large request, a "split" answer, and a triage call that fails or answers with anything but
the JSON asked for are planned in full; the last two print a notice. A verified one-step workflow
template that writes exactly the named files decides "one" with no call. A run started this way
is not made into a template.

The root gate of a small request runs in the background on each verified tip of `kgraf/<run>`,
one run at a time. A tip that is verified while it runs waits for that run to end; the next run
is on the newest tip, and older waiting tips are dropped. A failure after the merge is printed
as a notice and opens a remediation. The run completes when the gate passes.

**Early start.** With `--early-start` (on by default in the app), the one worker starts beside
the triage call when five conditions hold: it has a write scope (the named files' package, the
named files when another contract holds a lease in that package, or the repository when no file
is named and no other contract is live), a worker slot is free, the triage call is predicted
slower than kGraf's dispatch (from the route's measured triage calls, else 3 s for a hosted
route and 8 s for a local one), fewer than half of the past triage answers were "split", and
plan review would not show the contract first. Each unmet condition is in the trace (`triage`,
step `early_unmet`). The early worker may verify, but its work merges only after the answer.
In the Manual, Accept edits and Auto modes, a permission request it raises waits for the answer
too. "split" stops it, releases its worktree and leases, records what it used, and then plans
the request.

**Plan review.** In the modes that ask (Manual, Accept edits, Auto), plan review shows a small
request's one contract as its card, with no planning call behind it. Orchestrator decides and
Bypass never ask, so the contract runs without a card. With plan review on, no worker starts
before the triage answer.

**Resume.** A run stopped while its triage call was out is planned in full when resumed, with
no second triage call; an early worker it had started is cancelled first.

**Precedence.** Each setting is taken from the flag, then the run file (`plan`, `early_start`
in `.kgraf/run.json`), then the user setting (`plan_mode = "auto"`, `early_start` in
`settings.toml`), then the default: `full`, no early start. `kgraf schedule run-now` ignores
`plan_mode` and the early-start setting and plans in full.

The trace records each step as a `triage` event (`decision` with the time from the request,
`asked`, `answer`, `early_start`, `merge_held`, `early_cancelled`, `fallback`, `lost`). The
dispatch of a small request is measured from the request's receipt (`small_request_dispatch`,
or `triage_dispatch` when the triage call decided it).

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
A resume does not repeat a paid call. A note sent to a run that is not running (from the box or
`--interactive`) is saved and acknowledged as waiting; the next resume, or a reopen of a completed
run, takes it in first. A held or failed gate is run again, or waived, with
`--resume`, `--retry-gate` or `--waive-gate`, and `--gate-note`. `--resume` alone refuses a
completed run; reopen one with a change instead.

### Change a run

A change to a run's plan costs one `replan_delta` call against the plan the run already has,
not a new plan (decision 0100). The run's branch `kgraf/<run>` continues, and only what the
change adds or changes runs.

| Command | What it does |
|---|---|
| `kgraf orchestrate --resume RUN --change FILE ...` | Reopen a completed or stopped run with the change in FILE. |
| `kgraf orchestrate --resume RUN --edit FILE ...` | Reopen it with its instruction as you edited it. The lines that differ are the change; the planner reads the whole edited text. An edit that changes no line sends nothing and exits 0. |
| `kgraf orchestrate --instruction FILE --from-run RUN ...` | Start a new run from a copy of RUN's plan. A contract whose verified commit is already in the new run's base (you landed RUN), or whose merge changed nothing, is done; the rest remain. One `replan_delta` call applies the difference between RUN's instruction and FILE. Work in plain directories (no git) counts as remaining. RUN is read, never written. |
| `/edit` in the box | Open the run's instruction (the original and every applied change, as one text) in the composer; send to apply the edit as one change. It targets the run in this window, else the newest stored run, and names it. |
| `/edit --new` in the box | The same, as a new run copied from that run. |

A reopen makes no `decompose` call and no re-grounding call. The answer is one of: a delta (contracts
to add, change or remove), `no_change` (the planner's answer is printed as a notice), or
`replan_all` (the plan is left as it is and the change waits: a full plan runs only on your yes).
The call has reasoning off and an output cap of 600 tokens per contract the change names, at
least 1,200; an answer cut at the cap is asked once more with the cap doubled.

**What a change does.** Added contracts take the id `d<n>.<id>`, where `n` is the change's
number in the run (an id already taken is refused). A change that only adds text at the end of a
contract's instruction and acceptance amends it; any change to its files, dependencies, route or
check supersedes it with a new revision `d<n>.<id>`, and the contracts that depended on it depend
on the new revision. By the contract's state:

| State | Amended | Superseded | Removed |
|---|---|---|---|
| Not started, or held | the brief is rewritten; a hold clears | the old one closes as replaced; the new one is queued | closes as removed; never runs |
| Running (a stopped run's attempt that would resume) | a redirect waits in the worker's inbox; it sees it at its next round | the attempt is stopped: its transcript is kept, its worktree is released as your worktree setting says, its leases are released | the same |
| Verified | a follow-up `<id>.f<n>` that depends on it; contracts still to run that depended on it depend on the follow-up too | the same; verified work that depended on it is named as possibly stale and checked again in the background once the follow-up has merged; the run waits for that check, and a failure is a notice, never a block. A check the run's end cuts short is named | refused |

The merged plan passes the same checks a new plan does; a refused answer is asked once more
with the reason, and a second refusal leaves the plan unchanged. If a contract the change names
moved state while the change was planned, the answer is asked once more naming each conflict. A
change applies whole or not at all. Quality gates over a changed contract run again; the root
gate covers the added contracts. An added contract may depend only on verified work outside the
change: a dependency on a contract that was replaced, removed or never verified is refused by name.
The cap on new contracts per change counts follow-ups and new revisions too.

**Who must say yes.** Your permission mode decides, by what the change does to work: *additive*
(adds, amends, a follow-up), *reshaping* (supersedes or removes work that has not started), or
*cutting* (stops running work, or supersedes verified work).

| Mode | Additive | Reshaping | Cutting |
|---|---|---|---|
| Manual | asks | asks | asks |
| Accept edits | applies | asks | asks |
| Auto | applies | applies | asks |
| Orchestrator decides | applies | applies | applies, but refuses to supersede verified work with no contract that writes its files |
| Bypass | applies | applies | applies |

A change you wrote (`--change`, `--edit`, `/edit`, plan review's Change) is your yes for an
additive or reshaping change in every mode; a cutting one follows the mode. When the mode asks, the
box shows the change as a card (Yes applies it, No refuses it), and `--interactive` asks the same
on the terminal (`apply change N? [y/n]`). The answer is bound to that change's number: a card that
changed meanwhile is not answered by it. A card withdrawn before you answer (the run stopped, or
the mode stopped asking) gives the terminal back, so the next line you type is a note again. With
nobody to answer, or when the run's time limit runs out before you answer, the change waits
(pending) with a notice. A pending or refused change taken at a reopen, a copy or a resume stops
the run before anything is dispatched, so work the change would have altered does not run; the
run exits nonzero, naming each change and why. Refusing plan review's Change leaves the shown plan
as it was. Plan review itself also ends at the run's time limit, with nothing started.

Outside Orchestrator decides, a change that supersedes verified work with no contract writing its
files applies; a notice names those files (the verified code stays merged on the run's branch),
and the report counts it as verified work lost.

**Refused before anything is written.** A reopen of a run another process holds (a live run, or
a land in progress) is refused, and nothing is written: the change file stays as it was. So is a
run whose land journal is still open, and a land that stopped while git updated your checkout is
named so you can finish it first (`kgraf land`). In the box, the edit stays in the composer.

**A kill is safe.** Each step of a change is saved: received, planning, answered, applied (or
refused, or pending). A resume goes on from the saved step: an answer already saved is applied
with no new call, and a call that never answered is made once more. A change is never applied
twice or in part. The redirects a change sends to running work are saved with the change, and a
resume delivers each once.

**The report.** Each change has a line (`change  N (source, session S) state: what it did;
delta T tokens, plan P tokens (ratio of it)`): its delta tokens beside the tokens of the
`decompose` call that made the plan, so you can see what a change cost. A `changes` line counts
changes per working session, refused and pending changes, `replan_all` answers and verified
work lost. Delta tokens include calls that failed and calls cut by a stop. When you are at the
plan review card, its Change is a delta on the shown plan too, and a `replan_all` answer there runs
a full plan; with nobody at the card, Change runs a full plan as before. Re-grounding reads the original instruction
and every applied change. The trace records each step as a `change` event, and each reopen as a
`reopened` event; readers of the trace split working sessions there, so the time between them is
not run time.

With `--cache-layout`, each `replan_delta` call lists the plan's contracts in id order (so a
contract that closes rewrites only its own line), ends the plan block with a cache mark, and
sends the run's cache key. The mark is left out where the route takes none, where the plan is
shorter than the model caches, and where the run's previous change request is older than the
provider's cache lifetime. The mark costs a write at a higher rate than an uncached read, and
nothing is claimed about what it saves: the run report prints, per call kind, `cache read N% of
input` and the tokens written with the premium paid (`cache read share not reported` for a
vendor whose usage has no cache field), and the `changes` line counts the run's delta calls and
re-asks, so a run with the layout on can be compared with one off by calls as well as tokens.
Which routes take a mark or a key is in [MODEL-SUPPORT.md](MODEL-SUPPORT.md); for a Chat
Completions route that takes `prompt_cache_key`, name its route id in the run file's
`prompt_cache_key_routes`; a name that is not a route of the run, or not a Chat Completions
route, refuses the file. Triage, re-grounding and planning requests are not changed by it.

### A contract's own check

With `--own-checks` (Settings > General > "Let a contract carry its own check"), the planner may
give a contract a `check`: a command that fails when the contract's work is wrong. kGraf runs it
only when `--verify` tests nothing for that contract (the command checks nothing, like `true`,
or the narrowed check selects no test), and a pass verifies the contract. A check's exit status
must be able to fail because a test run, a build (`cargo test`, `cargo build`, `npm test`, `go
test`), or an assertion over a file the contract writes (such as `grep` on it) failed. `true`, `:`,
`exit 0`, `echo`, `test -f`, `test -e`, `ls`, `cat` and version or help requests (`cargo
--version`) check nothing; neither does a check whose status a trailing `|| true`, `; true` or
`exit 0` masks, or one that takes its status from a pipe's last command (`cargo test | tee log`,
unless `set -o pipefail` comes first). Substitutions, subshells and `if`/`for` blocks are not
judged: write plain commands joined by `&&`. The
planner hears about `check` only with `--own-checks` on; without it, no check is stored. A plan
copied with `--from-run` keeps a copied check only if it passes the same test, else a notice says
it was dropped. Where `--verify` does
test it, the check is not run. A check that checks nothing, or that runs a program your deny or
ask rules name, is refused when the plan is read, and the planner is asked again. Past your
rules, the mode decides: Bypass and Orchestrator decides take it; the other modes take it only
when every program it runs is in your allow set, since nobody is there to answer an ask. A
change to a contract's check supersedes the contract.

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

### The plan in the graph

Every run writes its plan into the project's graph as work nodes: a plan root `run:<run id>`,
one item for each contract (`run:<run id>/item/<contract>`), the dependencies between items, and
a record of each contract's history (the change that revised or removed it, the commit that
verified it). The run record stays the truth; the nodes are rows kGraf derives from it, written
in the same step as the record, so they cannot disagree after a stop or a crash. A contract keeps
its item for its whole life; a change that replaces a contract gives the replacement a new id.
The graph draws each contract once, from the record. The nodes are data you can query, not a
second drawing. A run's root node is drawn too, with its land state; selecting it takes you to
the orchestrator box, and its Land button is the same land as `/land`.

If a notice says the mirror was not written, the run went on and its record is saved. Run
`kgraf mirror rebuild RUN` to write the nodes again from the record. The command says whether
the mirror already matched (and writes nothing) or what it rewrote, with the time it took. The
record wins when the two differ: a node or fact the record does not give is marked removed or
closed (never deleted), so a second `rebuild` always says the mirror matched. When you resume a
run, kGraf compares the mirror with the record first and rewrites it only if they differ, also
for a run with nothing left to do. A dependency a revision drops stays as an edge of the old
state; an item's body holds its current dependencies. The live activity card shows one line for
a save's plan rows, not one for each.

### Worker worktrees

kGraf deletes a worker's worktree, with its build output, once nothing needs it. To keep
them, pass `--keep-worktrees` to `orchestrate` or `schedule run-now`. In the app, turn off
Settings > Runs > "Delete worker worktrees after use". `kgraf clean <worktree>` removes a
kept one. A worktree that a live worker, a stopped run or a pending merge needs is never
deleted. A worktree that cannot be deleted stays, and the report names it.

### Landing

A run's work collects on its own branch, `kgraf/<run>`; your branch moves only when you
land. `kgraf land --run RUN` (or `/land` in the app's box) lands it onto the branch the run
started from, or onto `--into BRANCH`:

- **The target.** With no `--into`, the target is the branch the run started from, and it must
  be the branch checked out now; otherwise kGraf refuses and names both. A run that recorded
  no start branch needs `--into`. The branch name must match exactly: on a file system that
  ignores case, `--into Main` does not mean `main`. A branch with a rebase or bisect in
  progress is refused. Every question kGraf asks names the target and the commit, with how
  many commits it brings.

- **What lands** is the newest commit of `kgraf/<run>` the root gate passed on (`--commit`
  names another). Work still in progress does not block it; it lands next time.
- **Always one merge commit**, even when a fast-forward is possible: your branch's old commit
  is the first parent, the run's commit the second. There is no squash. The message lists
  the verified contracts and ends with the trailers `kGraf-Run: <run>` and
  `kGraf-Gate: <commit> tree <tree>` (the commit and tree the gate passed on).
- **When your branch gained commits since**, the merge result is new code, so the root gate
  runs on it first. A conflict lands nothing; resume the run so a resolver merges your branch
  into `kgraf/<run>`, then land again.
- **No passed gate** (the gate never ran, failed, or failed on the merge result): kGraf warns
  loudly, names the gate and why, and lands only on your yes (`--untested`, or the prompt).
  The yes holds for that commit and that target as it was; if the target moved before you
  answered, kGraf warns again. `--untested` answers the question ahead of time, after the
  warning is printed.
  The merge commit then starts with `UNTESTED:`, carries the line
  `kGraf: UNTESTED - root gate <gate> did not pass: <reason>` and the trailer
  `kGraf-Gate: untested`. With nobody at the terminal, kGraf never lands untested.
- **No interface stub lands** without its verified producer: a commit that holds a stub
  whose producer has not verified is refused, naming the interface.
- **Your checkout.** When the target is checked out (in your main checkout or a linked
  worktree), kGraf moves it with your own git, `git merge --ff-only <merge commit>` in that
  worktree. This is the only step of a land that runs your hooks (`post-merge`) and filters
  (a smudge filter checks the landed files out filtered). Local edits to other files stay
  as they are. When a local change would be overwritten (modified, staged, or an untracked
  file in the way), git refuses and nothing changes. A target nobody has checked out moves
  only if it is still where kGraf found it. A detached HEAD, or a branch with no commit
  yet, is refused by name.
- **What a land runs is fixed when the run starts.** kGraf copies your hooks then, into its own
  run directory, and records every setting that names a program (filters, diff and merge
  drivers, the file-system monitor, signing). The land's checkout runs with exactly those:
  hooks or filters a worker committed or configured never run; yours (for example git-lfs) run
  as they were. kGraf asks only about what it cannot fix this way: a signing setting changed
  since the start, and a config file your git reads later (an included file in the work tree,
  `.lfsconfig`) that the merge changes. Each is listed with its file and blob; the yes holds
  for that list, commit and target, and is checked again right before the move.
- **A checkout git did not finish** (stopped at its time limit, or by a signal) can leave files
  partly written. kGraf says so and keeps the land open; `kgraf land --run RUN --recover` puts
  back the files it can tell apart, removes files the land added, removes a stale lock, and
  lists the rest. Then land again.
- **Signing.** When your `commit.gpgSign` is on, the merge commit is signed with your
  `user.signingKey` and `gpg.format`, without a prompt. If signing fails, nothing is
  committed and no branch moves; kGraf shows git's message. kGraf's own commits on
  `kgraf/<run>` are not signed.
- **Afterward** `kgraf/<run>` moves forward to include the merge commit, keeping any work
  verified after the landed commit; it never moves backward. A land interrupted between
  moving your branch and that step finishes on the next `kgraf land`, also when you have
  committed on top meanwhile. An error git reports after your branch moved (a hook killed at
  its time limit) does not undo the land; kGraf finishes it and shows the error.
- Landing needs git 2.42 or later; an older git is refused before anything is written. A
  run whose loop is live is landed from its box with `/land`, never from a second process;
  there the merge-result gate runs in the background, and the run keeps taking notes and
  answers. One land waits at a time. When that gate fails, kGraf merges your branch into
  `kgraf/<run>` and opens a fix contract there (a conflict lands nothing and says so). Your
  repository's first commit, made when kGraf starts in an empty repository, is signed the
  same way.

**Land suggestions.** kGraf suggests landing, and never lands without your yes, in every
permission mode. It asks when every contract is verified and the root gate passed on the
newest commit, when your branch gained commits while gated work waits, or when the session
closes with gated work not landed. In the app the suggestion is a card in the orchestrator
box (Land, Not now, each with a confirm step); at the end of a `--interactive` run it is a
`land?` line answered by `y` or `n`. At most one suggestion is open. After "Not now",
kGraf asks again only after new verified work and 15 minutes; declines are kept in the run
record. Benchmark runs never suggest a land.

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
needs `.kgraf/run.json`. `/status` asks where the run stands. Once kGraf has measured an
interaction, `/status` adds one short line per kind of interaction, in budget order, each the
newest split of its kind, that splits its time into kGraf overhead, model time and tool time (for example
`latency contract_merged (B10): 900.0 ms = kgraf 300.0 + model 600.0 + tools 0.0`). The run
report prints the same lines. `/compact` compacts the
orchestrator, and `/compact selected` or `/compact <worker>` targets a worker. `/edit` opens a
run's instruction in the composer to change it as one change, and `/edit --new` starts a new run
copied from it (see [Change a run](#change-a-run)). `/land` (or `/land into <branch>`) lands the
run onto your branch; see [Landing](#landing). `/help` lists them. Tool approvals are answered in
place: allow once, allow always, deny.

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
