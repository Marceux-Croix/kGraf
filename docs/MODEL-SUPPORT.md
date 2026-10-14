# Model support: what is verified and what is not

kGraf reaches models through **routes**. A route is one catalog entry: one vendor, one wire
(the request and response format), one address. This page lists every route and model kind
that the code builds, and says which ones have exchanged a real request and response with the
real service.

Read it before you pick a route for work that matters. A route marked BUILT, UNTESTED has
passed tests against recorded replies only. It may fail on its first live call.

## How the catalog agrees with this page

- `kgraf providers` prints a confidence word for each route (`verified`, `documented`,
  `unverified`). A generative route is `verified` there only where the table below marks
  it VERIFIED. The test `the_catalog_confidence_matches_the_recorded_live_exchanges` in
  `kgraf-fleet` fails if the catalog and the table disagree.
- A route moves to VERIFIED by one recorded live exchange. [CONTRIBUTING.md](../CONTRIBUTING.md)
  gives the steps.

## Model kinds

A route serves one or more **model kinds** (decision
[0089](decisions/0089-models-of-every-kind-are-routes.md)).

| Kind | What it returns |
|---|---|
| generative | Prose and tool calls. Runs workers and the planner. |
| typed-decision | A value inside a declared answer space, with a probability. |
| embedding | One vector per text. |
| rerank | A relevance score for each document against a query. |

A route with no claim for a kind does not serve it. A route serves a non-generative kind only
where the vendor documents that endpoint at the route's address.

## The table

Rows are grouped by kind. "Wire" is the request format. Dates are UTC.

| Kind | Route id | Vendor | Wire | Status |
|---|---|---|---|---|
| generative | `anthropic` | Anthropic | `anthropic-messages` | VERIFIED, recorded live exchange, 2026-09-28 |
| generative | `openai` | OpenAI | `openai-responses` | VERIFIED, recorded live exchange, 2026-09-26 |
| generative | `kimi-anthropic` | Moonshot (Kimi) | `anthropic-messages` | VERIFIED, recorded live exchange, 2026-09-26 |
| generative | `kimi` | Moonshot (Kimi) | `openai-chat-completions` | VERIFIED, recorded live exchange, 2026-09-26 |
| generative | `ollama` | Local (Ollama) | `openai-chat-completions` | VERIFIED, recorded live exchange, 2026-09-28 |
| generative | `deepseek` | DeepSeek | `openai-chat-completions` | BUILT, UNTESTED |
| generative | `kimi-code` | Moonshot (Kimi Code subscription) | `openai-chat-completions` | BUILT, UNTESTED |
| generative | `zai` | z.ai (GLM) | `openai-chat-completions` | BUILT, UNTESTED |
| generative | `qwen` | Alibaba (Qwen) | `openai-chat-completions` | BUILT, UNTESTED |
| generative | `grok` | xAI (Grok) | `openai-responses` | BUILT, UNTESTED |
| generative | `grok-chat` | xAI (Grok) | `openai-chat-completions` | BUILT, UNTESTED |
| generative | `google` | Google (Gemini, OpenAI-compatible layer) | `openai-chat-completions` | BUILT, UNTESTED |
| generative | `gemini` | Google (Gemini native) | Gemini `generateContent` | BUILT, UNTESTED |
| generative | `vertex-gemini` | Google Vertex AI (Gemini) | Vertex `generateContent` | BUILT, UNTESTED |
| generative | `vertex-anthropic` | Google Vertex AI (Anthropic) | Vertex `rawPredict` | BUILT, UNTESTED |
| generative | `bedrock` | AWS Bedrock | `bedrock-converse` | BUILT, UNTESTED |
| generative | `azure-openai` | Microsoft Azure OpenAI | Azure Chat Completions | BUILT, UNTESTED |
| generative | `azure-openai-responses` | Microsoft Azure OpenAI | Azure Responses | BUILT, UNTESTED |
| generative | `meta` | Meta (Meta Model API) | `openai-responses` | BUILT, UNTESTED |
| generative | `together` | Together (Llama hosting) | `openai-chat-completions` | BUILT, UNTESTED |
| generative | `llama-cpp` | Local (llama.cpp server) | `openai-chat-completions` | BUILT, UNTESTED |
| embedding | `openai` | OpenAI | OpenAI `/embeddings` | VERIFIED, recorded live exchange, 2026-09-29 |
| embedding | `google` | Google (Gemini, OpenAI-compatible layer) | OpenAI `/embeddings` | BUILT, UNTESTED |
| embedding | `ollama` | Local (Ollama) | Ollama native `/api/embed` | BUILT, UNTESTED |
| embedding | `llama-cpp` | Local (llama.cpp server) | OpenAI `/embeddings` | BUILT, UNTESTED |
| embedding | `tei` | Local (Text Embeddings Inference) | TEI native `/embed` | BUILT, UNTESTED |
| embedding | `azure-openai` | Microsoft Azure OpenAI | Azure embeddings | BUILT, UNTESTED |
| embedding | `azure-openai-responses` | Microsoft Azure OpenAI | Azure embeddings | BUILT, UNTESTED |
| rerank | `together` | Together (Llama hosting) | common rerank shape | BUILT, UNTESTED |
| rerank | `llama-cpp` | Local (llama.cpp server) | common rerank shape | BUILT, UNTESTED |
| rerank | `tei` | Local (Text Embeddings Inference) | TEI native `/rerank` | BUILT, UNTESTED |
| typed-decision | `systemone` | Local (`systemone` server) | `systemone` (`/v1/systemone`) | BUILT, UNTESTED |

Count by status: 6 VERIFIED (5 generative, 1 embedding) and 26 BUILT, UNTESTED (16
generative, 6 embedding, 3 rerank, 1 typed-decision).

Notes on single rows:

- `openai` embedding: one call of one text. Request batching, the vector-width check and
  per-model rate-limit header recording were added after it. Recorded replies test those.
- Rate-limit answers (hosted wires with an HTTP status): an adapter reads `retry-after-ms`, or
  `retry-after` in seconds, from a failed response (capped at 24 hours; an HTTP-date, a negative
  or a non-numeric value is not read), and the failed call keeps it with the HTTP status on its
  ledger row (`http_status`, `retry_after_ms`), beside the time the call waited for a rate-limit
  slot (`queued_ms`). The per-route limits the OpenAI-style and Anthropic-style header readers
  record are unchanged by it. Tested against recorded headers and a stub vendor; no route's
  status above changed, since none was re-verified live for this.
- `anthropic`: plain call, streamed call, tool round trip, prompt caching (cold write, then
  warm read), rate-limit headers, and a note injected after tool results all passed.
- `ollama`: the first exchange used `qwen2.5-coder:7b`, which returned its tool call as plain
  text (see "Known limits"). Later streamed and buffered calls with `qwen3.6:35b` are recorded
  in `crates/kgraf-provider-openai/tests/fixtures/`. The adapter keeps the thinking text Ollama
  sends as `reasoning` and records `cached_tokens` as cache reads.
- `systemone`: a local server that answers with a value from a declared set. The route has no
  live exchange. The client talked to a real server once, in a probe recorded under decision
  [0045](decisions/0045-the-local-decider-loses-to-the-frontier-and-the-seam-stays.md). That does
  not verify the route.
- `kimi-code`, `zai`, `qwen`, `grok-chat`, `meta`, `together`: the catalog and the record
  agree. Nobody has run them.
- `kimi`: Moonshot refuses a tool whose schema has a node with no `type`. kGraf checks its
  own tools for this. A tool from an MCP server keeps the server's schema, so such a tool
  fails the call with a message that names the refusal.

## What the statuses mean

### VERIFIED

One request and one response were exchanged with the real service, through the adapter as a
run builds it, and the exchange was recorded. The record shows the request, the reply, the
model the service named, the raw usage the service reported, and the ledger row settled.

VERIFIED covers what the record shows. It does not cover other models on the same route, other
features of the route (streaming, caching, reasoning summaries) that the record does not
name, or later code changes. The row notes say where a record is narrow.

### BUILT, UNTESTED

The adapter exists. Its tests use replies written from the vendor's documentation or copied
from the vendor's source. A mock repeats the assumption the adapter was written from, so
these tests cannot show that the service answers the way the documentation says.

Expect to check the raw usage the route records on the first live call. Three failure kinds
have been found on other routes: a field accepted and ignored with HTTP 200, a model name
silently replaced, and a usage shape that differs from the documentation. kGraf refuses
some of these before or after the call (model echo check, missing usage), and the call then
stays open in the ledger until you settle it with `kgraf unreconciled`.

## Known limits

Each item is a documented limit of a route that has no live exchange, unless it says
otherwise.

- **A buffered generative call waits for its whole answer** (every generative route). The
  time bound is the larger of the route's timeout and the output cap at 20 tokens per
  second (3,200 seconds at a 64,000-token cap). No live call has waited this long.
- **Google embeddings truncate silently** (`google`, embedding). Keep each text within the
  model's limit. Decision [0089](decisions/0089-models-of-every-kind-are-routes.md).
- **Google's OpenAI-compatible layer ignores unsupported parameters** (`google`,
  generative). Treat a capability there as not honored until a live call proves it.
  Decision [0090](decisions/0090-google-adapters.md).
- **Vertex schema and name questions are open** (`vertex-gemini`, `vertex-anthropic`). The
  tool-schema conversion, the 64-character function-name limit and the model echo come from
  the documentation. A live call has to confirm them.
- **Bedrock does not name the model** (`bedrock`). The echo check has nothing to compare.
  Decision [0091](decisions/0091-bedrock-converse-adapter.md).
- **Azure supports only the `api-key` header** (`azure-openai`, `azure-openai-responses`).
  Entra ID, sovereign clouds, Foundry project hosts, auto-updating deployments and
  model-router deployments do not work. Decision
  [0092](decisions/0092-azure-openai-transport.md).
- **Cohere rerank is not supported.** It bills in search units, which the token ledger
  cannot price.
- **Together embeddings are not claimed.** Its documented reply has no token count.
- **Text Embeddings Inference names no model** (`tei`), so the echo check cannot run.
- **The `ollama` route is verified against Ollama 0.34.4.** A model that needs a newer
  server fails the run. An older or unversioned server gets a notice and the run goes on.
- **Some local models return tool calls as text** (`ollama`). `qwen2.5-coder:7b` writes its
  tool call as JSON inside the reply. kGraf reads that as a finished reply, so a worker on
  that model has nothing to run.
- **A server can drop a tool call it cannot parse and answer 200** (`ollama`, seen with
  `qwen3.6:35b`). kGraf detects the lost output from the token count, tells the model its
  reply did not arrive complete, and allows two such replies an attempt.
- **Reasoning tokens are not split out on `ollama`.** Reports show "not reported", never a
  reasoning count of zero.
- **Reasoning level support differs by route** (the `reasoning` setting, see
  [USAGE.md](USAGE.md)). Measured on `ollama` with `qwen3.6:35b`, three requests per form
  (2026-10-08): `off` removed all reasoning (4 completion tokens, 0 reasoning characters);
  `low`, `medium` and `high` changed nothing. kGraf refuses those three levels there
  before any call. Every other route and model is BUILT, UNTESTED for every level: the
  level is sent and the run start says `UNVERIFIED`.
- **A forced tool choice is not honored on `ollama`.** After a worker ends a turn with no
  tool call on a contract that writes files, kGraf asks the next request to require a tool
  call. Ollama 0.34.4 drops the field (read from its source, 2026-10-10), so kGraf does not
  send it there and the worker relies on a notice that names the tool. Chat Completions and
  Anthropic Messages routes send it (BUILT, UNTESTED). Responses, Bedrock and Gemini
  refuse it.
- **A model name is not a verified model.** A VERIFIED row covers the models named in its
  record. DeepSeek is known to replace an unsupported model name with a cheaper one and
  return HTTP 200. The model echo check exists to catch this.

## References

- Catalog and kind claims: `crates/kgraf-fleet/src/lib.rs` (`CATALOG`, `KIND_CLAIMS`).
- Decisions: [0006](decisions/0006-provider-layer-shape.md),
  [0089](decisions/0089-models-of-every-kind-are-routes.md),
  [0090](decisions/0090-google-adapters.md),
  [0091](decisions/0091-bedrock-converse-adapter.md),
  [0092](decisions/0092-azure-openai-transport.md).
- Provider setup and commands: [USAGE.md](USAGE.md).
