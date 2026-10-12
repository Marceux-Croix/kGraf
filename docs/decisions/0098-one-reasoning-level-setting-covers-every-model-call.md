# Decision 0098: One reasoning-level setting covers every model call: a universal level, overrides by group or call kind, and a level measured to have no effect is refused

**Status:** Accepted (2026-10-08). Built.
**Deciders:** the project maintainer.
**Related:** 0006 (provider seam, capability traits), 0079 (interface review), 0089 (model kinds), 0095 (worker compaction).

This record exists so the reasoning setting is not re-argued. If a future contributor asks
"why does kGraf refuse `low` on this route," or "why is there no thinking level per route,"
the reasoning is here.

---

## Decision

1. **One setting, `reasoning`, sets the reasoning level of every generative model call a run
   makes.** It has a universal value (`all`) and overrides by group (`planner`, `checks`,
   `worker`) or by call kind (the ledger `call_kind`, list below). The most specific scope
   wins: the kind's own override, then its group's, then `all`, then unset.
2. **Each level is `unset`, `off`, `low`, `medium` or `high`.** `unset` sends no reasoning
   field, so the model's own default applies; it is the default everywhere. An explicit
   `unset` override beats a universal level.
3. **`off` means no reasoning. It never means a lower level.** `off` is sent only by an adapter
   with a wire form verified to turn reasoning off (Chat Completions:
   `reasoning_effort: "none"`), every answer is checked (`check_reasoning_off`; an answer that
   still reasons fails with `ReasoningNotOff` after the call is in the ledger), and an adapter
   with no verified form refuses the call before it is sent. kGraf never replaces a level with
   another one.
4. **Where it is set (precedence, highest first, scope by scope):** a command-line flag, a key
   in the project's `.kgraf/run.json` (the app's runs), the `[reasoning]` table of the user's
   `settings.toml`, the built-in default (unset). On the command line: `--reasoning LEVEL` (the
   universal level) and `--reasoning SCOPE=LEVEL`, repeatable, on `kgraf orchestrate` and
   `kgraf worker`; `kgraf schedule run-now` takes the user setting. A scope given twice in one
   layer is refused.
5. **`--planner-reasoning LEVEL` stays** and is the `planner` override. With
   `--reasoning planner=...` too, or in a run file with `reasoning.planner` too, it is refused
   (one of the two would be ignored).
6. **How kGraf knows whether a route honors a graded level: a measured route profile.**
   `kgraf_fleet::REASONING_MEASUREMENTS` records, per catalog route, model and level, whether a
   recorded measurement found the level honored or not honored. Rows are added only from a
   recorded measurement and name the record. Today it holds one route and model: `ollama` with
   `qwen3.6:35b` (Ollama 0.34.4, 2026-10-08): `off` honored; `low`, `medium` and `high` not
   honored.
7. **What happens to a level, per route, when the run opens** (`Host::open`; `kgraf worker`
   before its first call).
   - A graded level measured as not honored on that route and model is **refused**, before any
     call, with an error that names the route, the level, the call kinds and the record, and
     says to set those kinds to `unset` or `off`. Sending it would be an ignored parameter, and
     the default amount of reasoning in its place would be a silent substitution.
   - A graded level with no measurement for the route and model is **sent, with a recorded
     notice** at each run's start (`Notice::Reasoning`, printed as a `reasoning` line and shown
     in the app's run notices): the route, the level, the call kinds, and `UNVERIFIED`: no
     answer shows whether a server honors a graded level, so it may have no effect.
   - A graded level measured as honored is sent, and the notice names the record.
   - `off` is said at run start with its per-answer check.
   - The effective setting is printed with its source, one `reasoning` line per scope, beside
     the `limits` lines.
8. **Every token stays attributed.** The level is set inside the two places every generative
   request is built (`ModelCaller::request` for the orchestrator's calls, the worker loop for
   rounds and compaction summaries); the calls go through the same ledgered paths as before.
   Reasoning tokens are counted as they were (`reasoning_output`, or
   `unsplit_reasoning_chars` where a server reports one total).

## The processes and call kinds

Taken from the ledger `call_kind` strings and the call sites. A test sweeps the orchestrator's
code for every kind it asks a model with and fails if one is not in this list, or if the list
names a kind no code asks with.

| Scope group | Call kind | What the call is | Route | Call site |
|---|---|---|---|---|
| `planner` | `decompose` | The run's plan | Planner | `ModelDecomposer::decompose` (`plan.rs`) |
| `planner` | `interfaces` | The interface contracts of a plan (with contract checks) | Planner | `ModelDecomposer::decompose` (`plan.rs`) |
| `planner` | `reground` | Whether the next contract still serves the instruction | Planner | `ModelDecomposer::reground` (`plan.rs`) |
| `planner` | `answer_question` | The orchestrator answers a worker's question | Planner | `plan.rs` |
| `planner` | `judge_connections` | The orchestrator judges a proposed connection | Planner | `plan.rs` |
| `planner` | `interpret` | Reading the corpus into a plan (`--corpus`) | Planner | `ModelAsker` (`intake.rs`) |
| `planner` | `question_route` | Classifying a user's question | Planner | `question_route.rs` |
| `planner` | `question_answer` | The orchestrator answering a user's question | Planner | `question_route.rs` |
| `planner` | `challenge_review` | Review of a challenge to an interface (with contract checks) | Planner | `ModelReviewer` (`challenge.rs`) |
| `checks` | `handshake` | A party's statement of an interface | Each worker route | `RoutedHandshaker` (`checks.rs`) |
| `checks` | `consumer_test` | A consumer's own test of its producer | Each worker route | `RoutedConsumerTestWriter` (`consumer_tests.rs`) |
| `checks` | `interface_review` | Cross-vendor review of a delivery (`--interface-review`) | Each worker route | `RoutedInterfaceReviewer` (`interface_review.rs`) |
| `worker` | `worker_round` | One round of a worker's loop | The worker's route | worker loop (`kgraf-runtime`) |
| `worker` | `worker_compaction` | A worker's compaction summary | The worker's route | worker loop (`kgraf-runtime`) |

Not covered, on purpose:

- **Non-generative calls** (`embed`, `rerank`, `decide`, decision 0089). They are not
  completion requests and have no reasoning control.
- **`kgraf call`**, the one-shot diagnostic call. It sends exactly the request the user typed;
  its ledger row has no `call_kind`. A `--reasoning` flag there would make it a probe command
  for new measurements.
- **The benchmark brief runner** (`kgraf-bench-briefs`) sends no reasoning field: it is the
  measured baseline.

## Context

Before this record, `--planner-reasoning` set the level of the orchestrator calls made through
the planner's caller only. Workers, the contract-check calls and compaction summaries always
sent no field.

A measurement on Ollama 0.34.4 with `qwen3.6:35b` found that `reasoning_effort: "none"` turns
thinking off (0 reasoning characters on every planner call that asked for it, completion tokens
down 78% on matched calls) and that `low`, `medium` and `high` have no measurable effect
(reasoning lengths within the no-field range on two prompts). A server that ignores a graded
level answers with HTTP 200 and the default amount of reasoning; nothing in the answer says so.

## Options considered

**Where the knowledge of "is this level honored" lives:**

- *A per-adapter declaration.* Rejected as the source of truth. An adapter knows a protocol,
  not a server or a model: the Chat Completions adapter serves Ollama, llama.cpp, DeepSeek,
  z.ai and others, and the measurement shows that the same wire form is ignored by one model on
  one server. The adapters keep what they can know: Anthropic and Bedrock refuse every level,
  Responses and Gemini refuse `off`, Gemini refuses a model family it cannot map.
- *A measured route profile.* Chosen. It is data, keyed by route and model, which is the unit
  the measurement covered, and it is reviewable in one table next to the catalog. Its weakness
  is coverage: one row set today.
- *A probe command* that sends a pair of requests and compares reasoning lengths. Not built
  now. A single probe is noisy (the no-field requests ranged from 634 to 1,150 reasoning
  characters on one prompt), so a sound probe needs repeats and two prompts; it also costs
  tokens at every run start. It is the right next step to fill the profile, as a command the
  user runs once per route and model, not a run-start gate.

**What happens when a graded level is not known to work:**

- *Refuse every unverified graded level.* Rejected. No route has a graded level measured as
  honored, so this would make `low`, `medium` and `high` unusable everywhere.
- *Run with a recorded notice when unmeasured; refuse when measured as not honored.* Chosen.
  An ignored parameter must raise: where kGraf knows the level is ignored, it raises before any
  call. Where it does not know, it says so at every run's start, naming each route, level and
  call kind, so nothing is silent.
- *Fall back to `unset` or a lower level on a route that does not honor the asked one.*
  Rejected: that is a silent substitution.

**The setting's shape:**

- *Per route* (a level for each `route:model`). Rejected for now: a route is already chosen per
  process. A route that cannot honor a level is handled by item 7.
- *Flat per call kind only.* Rejected: 14 keys to set one behavior. The three groups match the
  routes the kinds run on (the planner's route, each worker route for the checks, the worker's
  own route), so a group override is the common case.
- *A map in each layer, merged scope by scope.* Chosen. A user can set `all = "low"` in the
  settings file and override one kind for one run with a flag, without restating the rest.

## Consequences

- The run file and the user setting gain a `reasoning` key; an old file without it means what
  it meant. The desktop app carries the `[reasoning]` table through a settings save unchanged;
  a control for it is later work.
- A run whose setting asks a measured route for a level it does not honor does not open. The
  message names the fix.
- Each new measurement (another model, another server, a graded level honored) is one row in
  `REASONING_MEASUREMENTS` and one row in `docs/MODEL-SUPPORT.md`, with the record.
- Tests prove the mechanism on the wire: each of the 12 orchestrator kinds and both worker
  kinds send their configured level in the Chat Completions body; an override beats the
  universal level; a not-honored level is refused at open; an unmeasured one is said.
- Reopens if: a probe command exists (item 6), a route needs a different level per route rather
  than per process, or refusing unverified graded levels is preferred.

## Amendment (2026-10-10): a forced tool choice follows the same rule

A worker whose model announces an action and ends its turn now asks, on the round after the
notice, for a forced tool call (decision 0096, amendment of 2026-10-10). Like a graded
reasoning level, the field is one an answer cannot prove was read: a server that drops it
answers 200 as if it were absent. It follows item 7's rule, per route and model, at the round
that would carry it:

- Recorded as ignored: not sent. The worker records why in a `WorkerEvent::ToolChoice` event
  that names the record. The run still opens: the forced choice is the worker's own fallback,
  not a setting the user asked for, so leaving it out changes nothing the user chose.
- Nothing recorded: sent, and the event says "UNVERIFIED".
- Recorded as acted on: sent, and the event names the record.
- An adapter with no wire form for it never sends it and refuses a request that asks for it
  (`kgraf_provider::check_tool_choice`). Chat Completions sends `tool_choice: "required"`,
  Messages sends `tool_choice: {"type": "any"}`; Responses, Converse and Gemini have none built.

The records are `kgraf_fleet::TOOL_CHOICE_MEASUREMENTS`, keyed by route and, optionally,
model: whether the field is read is the server's doing, so a row may cover every model on a
route. A row may come from a recorded live exchange, or from the server's own source when it
shows the field is never read; a source read can record "ignored" and never "acted on", and it
is deterministic where a sample is not. The one row: `ollama`, every model, ignored. In Ollama
0.34.4, `openai/openai.go`'s Chat Completions request type has no `tool_choice` field and the
handler binds the body with gin's `ShouldBindJSON`, which drops fields it does not know
(read 2026-10-10; `docs/MODEL-SUPPORT.md`, Known limits). A newer Ollama that reads the field
needs a new record before kGraf sends it there.

Tests: `kgraf-fleet` `tool_choice.rs`; `kgraf-runtime` `announce_without_tool_call.rs`;
`kgraf-provider` `a_forced_tool_choice_is_refused_where_it_cannot_be_sent`.

## Amendment (2026-10-12): a reasoning-only turn turns reasoning off for the rest of the attempt

A thinking model can end a turn with reasoning and no tool call or text. Two such turns in a
row failed the attempt. The fallback keeps item 3's rule: `off` is sent only where an adapter
has a verified form, and every answer to it is checked.

- At the first empty turn whose reply holds reasoning, on a route whose adapter declares it can
  turn reasoning off (`Provider::turns_reasoning_off`; Chat Completions only today), and when
  the configured worker-round level is not already `off`, the worker keeps the continue notice,
  adds a `ReasoningOff` notice ("the model ended a turn with reasoning only; the next request
  runs with reasoning off"), and sends every later round request of that attempt with `off`.
  This turn gets its continuation even when an earlier empty turn had one.
- Bound: that attempt only, and a compaction does not clear it. A new attempt is a new worker
  and starts at the configured level. Compaction summary calls keep their own level.
- No off level on the route: behavior is unchanged, with no notice.
- A server that ignores the off request fails the call with `ReasoningNotOff`, as for any off
  request. No substitute is made.

Tests: `kgraf-runtime` `worker_loop.rs` (`a_reasoning_only_turn_is_followed_by_a_request_with_reasoning_off`,
`a_route_without_an_off_level_keeps_the_empty_turn_behavior`,
`the_next_attempt_starts_at_the_configured_reasoning_level`).
