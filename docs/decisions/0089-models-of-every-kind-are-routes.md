# Decision 0089: Models of every kind are routes; the orchestrator decides their use

**Status:** Accepted
**Related:** decisions 0006, 0039, 0040, 0041, 0042, 0043, 0045, 0071, 0080

## Context

The provider seam was generative-only. A run could not hold an embedding model for
similarity, a reranker for ordering search results, or a System-One model for cheap typed
questions, though each is a model a user may already run beside their chat models. kGraf
takes all inputs, world models included, and always will: a model of any kind is something a
user can drop in the way they drop in a chat model, over the connection protocol it already
speaks. When to use one is the orchestrator's decision; the point is that the resource exists.

## Decision

1. **A model kind on every route, open.** `kgraf_core::ModelKind` names what a route's model
   returns. It is an open name checked against a registry (`kgraf_core::KINDS`), not a closed
   enum. The kinds shipped:
   - `generative`: prose and tool calls (the Messages, Responses and Chat Completions wires).
     The default: a route declared without a kind is generative.
   - `typed-decision`: a value inside a declared answer space with a probability, over
     `/v1/systemone`, wire unchanged (decision 0042).
   - `embedding`: OpenAI-style `POST /embeddings`, `{model, input: [...]}` to
     `data[].embedding` and `usage`.
   - `rerank`: the common `POST /rerank`, `{model, query, documents, top_n}` to
     `results[].{index, relevance_score}` and `usage`.
2. **One table per layer.** A kind is a row in four tables: `KINDS` (core: name, summary,
   tool names), a call trait (`Provider`, `Embedder`, `Reranker` in `kgraf-provider`;
   `Decider` in `kgraf-decide`), `KIND_BUILDERS` (fleet: which catalog routes serve it and how
   one is built into a `ModelRoute`) and `MODEL_TOOLS` (tools: the tool that reaches it).
   `ModelRoute` holds the call handle and returns it only for the kind asked for. A new kind
   adds one row to each and touches nothing else. A new generative wire needs no kind row:
   the generative row serves every wire another kind does not claim.
3. **Declared where routes are declared, credentialed the same way.** `--route` and
   `--planner` stay generative-only. Every other kind is `kind=route:model`: `--model-route`
   on `kgraf orchestrate` and `kgraf schedule run-now`, `model_routes` in `run.json`, shown
   with its kind in the desktop's model list and in `kgraf providers`. The route is a catalog
   route and its credential is that route's own variable, registered with the scrubber
   (decision 0071). The catalog gains a local `systemone` route, the only one serving typed
   decisions. Embedding and rerank are served by the OpenAI-family routes at
   `<base_url>/embeddings` and `<base_url>/rerank`.
4. **Every call is accounted.** Each kind's trait takes a ledger `CallToken`. Embedding and
   rerank calls settle their input tokens with the raw usage blob; a reply without usage
   raises and leaves the row open, never settled at zero. Priced where the price table has
   the model; unpriced otherwise, never $0 and never refused.
5. **The orchestrator decides.** The decomposition request lists the run's model routes by
   kind. The tools `decide`, `embed` (vectors, or texts ranked against a query by cosine
   similarity) and `rerank` join every worker environment of a run that declares a route of
   their kind; a contract names them in its `tools` like any other tool; the orchestrator
   calls them itself through `ModelKindCalls`. Nothing uses them by default.
6. **Kinds never cross.** A kind prefix on `--route` or `--planner` is refused; a model route
   without a non-generative kind is refused; a route that does not serve a kind is refused
   for it; a tool asked for a route of another kind returns the mismatch as an error.
   Nothing is rerouted.
7. **Decision 0045 still holds.** No decider supplies the collision expectation handshake. A
   typed answer is well formed, not verified.

## Verification status

- **embedding on the `openai` route: verified** with one live call to `/v1/embeddings`
  (`text-embedding-3-small`, input `["kgraf"]`): 1,536 dimensions, model echoed, two input
  tokens settled through the ledger.
- embedding on every other route: unverified (recorded-reply tests only).
- rerank: unverified (replies written from the vLLM and Jina documentation).
- typed-decision through the `systemone` route: unverified (recorded `/v1/systemone` replies).

## Options considered

- **One wide model trait with a kind flag.** Rejected by decision 0006: each implementation
  would stub the others.
- **A closed enum matched in each crate.** Rejected: a new kind would touch every crate.
- **A dedicated catalog entry per endpoint.** Rejected: every host serving `/embeddings`
  would need a twin entry and a second copy of its credential rule.
- **Mixed kinds in `--route`.** Rejected: every consumer of the worker roster would have to
  filter by kind, and one that forgot would send generative work to an embedding model.
- **A per-contract tool grant enforced at approval.** Rejected for now: the shared prefix
  (decision 0043) is one per vendor and model, and a per-contract tool set would split it.
- **An existing client crate.** `async-openai`, `openai-api-rs` and `rig-core` call
  `/embeddings` but tie the call to their own client and usage types (decision 0006), and none
  serves `/rerank`; `fastembed` runs models locally. The adapters are new.

## Consequences

- A run that declares no model route is unchanged.
- A run that declares one adds its tools to every worker's shared prefix, and one paragraph to
  the decomposition request.
- Rate limits do not apply to model routes.
- A kind whose output is not counted in tokens extends the usage form; it does not reopen the
  routing rules above.

## Amendments (2026-09-29)

Each point supersedes the text above where they differ.

- **Kind claims per route, from the vendor's documentation.** `KIND_CLAIMS` (fleet) lists the
  kinds each catalog route serves, with the vendor page it rests on; a route with no row
  serves generative work only. `KIND_BUILDERS` rows say only how a kind is built. Claimed:
  embedding on `openai`, `google`, `together`, `ollama`, `llama-cpp` and `tei`; rerank on
  `together`, `llama-cpp` and `tei`; typed-decision on `systemone`. Not claimed, as no
  endpoint is documented at the route's base URL: `kimi`, `kimi-code`, `deepseek`, `zai`,
  `qwen`, `grok`, `grok-chat`, `meta`. Azure OpenAI embeddings are not served yet: the path
  is per deployment and needs its own transport mode.
- **A new kind is rows and a trait, with no edit to existing code.** `ModelCalls::open` is the
  one call path for every kind: it finds the route by kind and name, takes the handle for the
  kind, opens the ledger row and takes a limiter slot. `ModelKind::spec` raises for a kind
  without a registry row.
- **Routes are found by kind and name.** One `route:model` may be declared as two kinds.
- **Dialects.** Embedding: OpenAI's shape; Ollama's native `POST /api/embed` with
  `truncate: false` (its `/v1/embeddings` cuts an over-long input without a word); Text
  Embeddings Inference's native `POST /embed` with `truncate: false`. Rerank: the common
  shape; TEI's `{query, texts}` to `[{index, score}]`. TEI reports tokens in the
  `x-compute-tokens` header and names no model, so the model echo cannot be checked on it. A
  local `tei` route is added. Cohere's `/v2/rerank` is not served: it bills in search units,
  which the token ledger cannot price, and its reply names no model.
- **Input limits.** An embedding input larger than one request is split into requests within
  the route's bounds, one ledger row each: 2,048 inputs per request (OpenAI), a conservative
  100,000-token bound on the summed input (enforced on bytes, since every tokenizer in use
  spends at least one byte per token), and 32 inputs on `tei`. An empty text is refused
  before sending. Vectors of mixed widths, or an empty vector, raise. A rerank reply with
  more rows than asked for is cut to the best rows with a notice.
- **Bounded tool results.** `embed` takes at most 32 texts and returns raw vectors only while
  they fit in 32 KB, else the texts' pairwise cosine similarities. `rerank` takes at most 256
  documents. `embed` takes optional `query_prefix` and `text_prefix`. A route's error reply
  reaches a model scrubbed of secrets and quoted as data. `decide`'s schema states its
  bounds, and a field of another answer space is refused.
- **The orchestrator's own model may call them.** When the run declares a model route, the
  decomposition request offers the model tools to the planning model, and each call it makes
  is run and accounted to the orchestrator before it answers, for at most four rounds. The
  tools are in every worker's tool set (one tool set, one shared prefix per vendor and
  model), and naming one in a contract's `tools` grants nothing more.
- **Rate limits apply to model routes.** `--rate-limit` may name a route used only as a model
  route; its calls take a slot on the same limiter the route's generative calls take. Rate
  limit headers and 429 replies on embedding and rerank calls are recorded as for chat.
- **Costs are summed exactly and rounded once.** A call is priced exactly in pico-dollars;
  ledger totals sum those and round once to micro-dollars, and a priced call that cost
  something is shown as at least $0.000001, never $0.
- Every route and dialect added or changed here is tested against recorded replies only.

## Further amendments (2026-09-29)

Each point supersedes the text above where they differ.

- **A claim rests on what the vendor's page shows, and carries its confidence.** A
  `KIND_CLAIMS` row now has a `confidence`: `verified` only where a call of that kind was
  exchanged with the live service (embedding on `openai`), `unverified` everywhere else. A
  claim whose documented reply lacks what the adapter needs is removed, since every call
  would bill and then raise. Changes:
  - `together` no longer claims embeddings: its reply schema (`EmbeddingsResponse`,
    <https://docs.together.ai/reference/embeddings-2>) has no `usage`. Its rerank claim stays,
    as its reply documents `usage.prompt_tokens`.
  - `together` rerank: the reference's example answers a request for
    `Salesforce/Llama-Rank-V1` with `"model": "salesforce/turboranker-0.8-3778-6328"` and states
    no rule relating the two (<https://docs.together.ai/reference/rerank-1>). Its claim sets
    `EchoRule::NotCompared`: the reply's model name is not compared on that route. Every other
    route still raises on a reply naming another model.
  - `google` embeddings: the cited page shows the endpoint and request only, not the reply's
    `model` or `usage`, so the claim is unverified in those respects. The embeddings guide
    says input over the model's limit is truncated without an error, and the
    OpenAI-compatible body has no field to refuse it. No per-request text count is published
    for this surface; the claim bounds a request at 100 texts, below the 250 the Vertex AI
    page states, so a smaller batch costs one more request rather than a refused one.
  - `llama-cpp`: the README shows the endpoints and requests only; the reply shapes (the
    requested `model` echoed, `usage.prompt_tokens` and `total_tokens`) are cited from the
    server's source (`tools/server/server-common.cpp`). Unverified.
  - `tei` rerank takes at most 32 documents per request, its default
    `--max-client-batch-size`, as its embed requests do.
- **Rerank requests respect the route's cap.** `KindClaim::rerank_max_documents` and
  `Reranker::max_documents` (default 256) bound one rerank request; `ModelCalls::rerank`
  refuses more before a ledger row is opened, the adapter refuses more before sending, and the
  `rerank` tool's description names every route with a lower cap.
- **Raw vectors are written from their `f32` values.** Widened to `f64` first, a
  1,536-dimension vector measured about 33 KB, over the 32 KB result bound, so even one vector
  came back as a similarity matrix. Written as `f32` it measures about 19 KB.
- **Model-tool spend is in the run's spend.** The embed, rerank and decide calls made by the
  planner, and those made by a worker, opened ledger rows that never reached the run's spend
  book: the reported spend left them out, and a resume, which seeds the book from the ledger,
  added them back. The orchestrator now charges them by the rule a resume seeds by (a settled
  call at its price, a failed one at zero, an unreconciled one at its worst case, an unpriced
  one noted and not charged) and marks them seeded. A worker's tool calls are added to its
  charge but not to the route's measured cost.
- **Round statistics leave out non-generative calls.** An embedding, rerank or decision call
  shares its route's limiter and so its `route` column, but it is not a round; counted, a
  flood of small fast calls pulled `tokens_per_round` and `round_ms` down and let capacity
  admit more workers than the route can serve. The row's call kind
  (`ModelKind::call_kind`: `embed`, `rerank`, `decide`) tells them apart.
- **Model-route rate limits have their own keys.** OpenAI states limits per model, so the
  embedding and rerank adapters record headers under `scoped_key(base_url, model)`, never
  under the base URL the chat adapter records the chat model under; a 429 is recorded under
  both. Capacity reads every model route's key (`kgraf_fleet::kind_limit_key`) beside the
  route's chat keys, takes the tightest limit and the newest 429, and reports a route used
  only as a model route.
- **The planner's tool loop is bounded per round.** At most
  `MAX_MODEL_TOOL_CALLS_PER_ROUND` (8) tool calls run per round; the rest are answered as not
  run. Before the last round the planner is told no tool calls remain, and a tool call in that
  round is refused unrun. The provider seam has no `tool_choice`, so the last round still
  lists the tools. The planner's hold covers every round it may take.
- **Failures and stops reach the run.** A model tool call the planner makes that fails is kept
  as a run notice. The run's stop (`OrchestratorHandle::stop`) cancels a model tool call in
  flight; a worker's model tool calls follow the worker's own cancellation.
- **Zero usage is missing usage.** A reply counting zero tokens for non-empty input (Ollama's
  `prompt_eval_count`, a usage object, TEI's header) raises `UsageMissing` and leaves the row
  open, as an absent count does.
- **Small calls are not inflated when seeded.** Seeding a resumed run's spend, and the
  admission's trailing hour, apportions each group's exact sum rounded once
  (`ExactMoney::apportion`) instead of rounding each call up to a micro-dollar. Ten calls of a
  fifth of a micro-dollar seed two micro-dollars, not ten. `Ledger::spend_by` still rounds
  each group on its own, so a group that spent anything never shows $0; its groups may sum
  above the total by under one micro-dollar per group.
- **What a new kind takes** (restating the earlier amendment, which said "rows and a trait,
  with no edit to existing code"): additive rows plus a trait, a tool and a method. A
  `KINDS` row and constant; a call trait; a `KIND_BUILDERS` row with its builder function and
  the kind in the claiming routes' `KIND_CLAIMS` rows; a `MODEL_TOOLS` row, its tool, and a
  typed method on `ModelCalls` that goes through `ModelCalls::open`. No existing match over
  kinds changes.
- **Every model-route row names its catalog route**, with or without a limiter, so a route
  priced only under its route-scoped key (Azure, decision 0092) is never priced by a bare
  model entry.
- **Azure OpenAI embeddings are served** through the Azure transport (decision 0092):
  `azure-openai` and `azure-openai-responses` claim the embedding kind, unverified.
- **`decide` on Vertex.** The Vertex schema subset has no `uniqueItems`, and its converter had
  refused the `decide` tool for it. The converter now keeps `uniqueItems: true` as a sentence
  in the node's description; the tool still checks for duplicate options when called.
- **Verification, stated exactly.** The one live call (embedding on `openai`, 2026-09-29 20:54
  UTC) sent one text and checked the request shape, the model echo, the vector and the usage
  settlement. It predates request batching, the vector-width check, per-model header
  recording and the zero-usage rule, which are tested against recorded replies only. No live
  call was made for these amendments.
