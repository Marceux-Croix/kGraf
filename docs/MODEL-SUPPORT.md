# Model support: what is verified and what is not

kGraf reaches models through **routes**. A route is one catalog entry: one vendor, one wire
(the request and response format), one address. This page lists every route and model kind
that the code builds, and says which ones have exchanged a real request and response with the
real service.

Read it before you pick a route for work that matters. A route marked BUILT, UNTESTED has
passed tests against recorded replies only. It may fail on its first live call.

The state of each route in your own build is also shown by `kgraf providers`.

## The short version

- **VERIFIED** means one live request and its response were exchanged with the real service
  and recorded. Six rows have this status.
- **BUILT, UNTESTED** means the adapter exists and passes tests against recorded or documented
  replies. No live exchange is recorded. Twenty-six rows have this status.
- The table has 32 rows: one per route and model kind.
- `kgraf providers` prints a confidence word for each route (`verified`, `documented`,
  `unverified`). In four places it differs from this page. See "Where the catalog and the
  record differ".
- A route moves to VERIFIED by one recorded live exchange. See "How a route becomes VERIFIED".

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
| typed-decision | `systemone` | Local (System-One server) | `systemone` (`/v1/systemone`) | BUILT, UNTESTED |

Count by status: 6 VERIFIED (5 generative, 1 embedding) and 26 BUILT, UNTESTED (16
generative, 6 embedding, 3 rerank, 1 typed-decision).

Notes on single rows:

- `openai` embedding: one call of one text. Request batching, the vector-width check and
  per-model rate-limit header recording were added after it. Recorded replies test those.
- `anthropic`: plain call, streamed call, tool round trip, prompt caching (cold write, then
  warm read), rate-limit headers, and a note injected after tool results all passed.
- `ollama`: the exchange used the model `qwen2.5-coder:7b`. That model returned its tool call
  as plain text. See "Known limits".
- `systemone`: the route has no live exchange. The typed-decision client itself talked to a
  real local server once, in the probe recorded under decision
  [0045](decisions/0045-the-local-decider-loses-to-the-frontier-and-the-seam-stays.md). That does not verify
  the route.
- `kimi-code`, `zai`, `qwen`, `grok-chat`, `meta`, `together`: the catalog and the record
  agree. Nobody has run them.
- `kimi`: Moonshot validates tool schemas strictly. A schema node with no `type` (an `enum`
  alone) is refused with HTTP 400 ("is not a valid moonshot flavored json schema, details:
  <At path 'properties.evidence.properties.kind': type is not defined>") before the model
  runs, for every request that offers the tool; the same request with `"type": "string"`
  added was answered (two live calls, 2026-09-30). kGraf refuses a built-in tool whose schema
  has an untyped node when the tool set is built; the check walks every keyword that holds a
  subschema (`$defs`, `patternProperties`, `not`, `if`/`then`/`else`, `contains` and the
  rest) and follows a local `$ref`, and a `$ref` it cannot resolve counts as untyped. An MCP
  server's schema is registered as its
  server gives it, so such a schema still fails the call, and the worker's failure names the
  refusal.

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

### How a route becomes VERIFIED

1. Set the route's credential in your shell. Never write a key into a file.
2. Send one real request and read one real response through the adapter, built the same way a
   run builds it. For a generative route, `kgraf call --provider <route> --model <model-id>
   --record <file>` does this. For a non-generative kind, use the kind's tool in a run, or an
   ignored live test as `crates/kgraf-fleet/tests/live_embeddings.rs` does.
3. Check the model echo, the reply, the raw usage, and the ledger row.
4. Record the exchange: the date, the model, the request as sent, the raw reply, and what it
   showed.
5. Change the route's confidence in the catalog, and update this page.

A route that fails this check stays BUILT, UNTESTED. The failure is recorded and the adapter
is fixed first.

## Where the catalog and the record differ

The catalog (`crates/kgraf-fleet/src/lib.rs`) and the recorded exchanges disagree in these
places. This page follows the record: it marks VERIFIED only where a live exchange is
recorded.

| Route | Catalog says | The record says | This page |
|---|---|---|---|
| `deepseek` | `Verified` | No recorded live exchange. An early implementation record lists it as verified without one. | BUILT, UNTESTED |
| `kimi` | `Documented`. The note says the cache-read field is not yet seen on a warm call. | Live calls on 2026-09-26 (plain, then streamed) and a later run on 2026-09-28. The 2026-09-28 run shows a cache read of 256 tokens on a second call. | VERIFIED |
| `kimi-anthropic` | `Documented` | Live calls on 2026-09-26 and 2026-09-28. Model echoed. The `/v1` root is correct. | VERIFIED |
| `ollama` | `Documented` | Live exchanges with a local Ollama server on 2026-09-28, in a benchmark run and a direct check of the request shape. | VERIFIED |

Three more generative entries carry `Documented` with no live record: `grok`, `google` and
`llama-cpp`. `Documented` means vendor pages describe the endpoint and protocol, and no more.
This page marks them BUILT, UNTESTED. `google` also has a confirmed defect. See "Known
limits".

The catalog's kind claims agree with the record. `openai` embeddings is the only `Verified`
claim. Every other claim is `Unverified`.

## Known limits

Each item is a documented limit of a route that has no live exchange, unless it says
otherwise.

- **A buffered generative call waits for its whole answer** (every generative route; the
  Chat Completions, Responses, Anthropic, Bedrock and Gemini adapters). The response
  headers of a call that is not streamed arrive when generation ends, so the bound on
  them is the larger of the route's timeout and the output cap at 20 tokens per second
  (3,200 seconds at a 64,000-token cap). A mock server checks this for Chat Completions.
  No route has had a live call that waited this long.

- **Google embeddings truncate silently** (`google`, embedding). Input over the model's
  limit is cut with no error, and the OpenAI-compatible body has no field to refuse it.
  Keep each text within the model's limit. Decision
  [0089](decisions/0089-models-of-every-kind-are-routes.md).
- **Google's OpenAI-compatible layer ignores unsupported parameters** (`google`,
  generative). A setting that looks applied can do nothing. Treat every capability there as
  not honored until a live call proves it. Decision
  [0090](decisions/0090-google-adapters.md).
- **Vertex schema and name questions are open** (`vertex-gemini`, `vertex-anthropic`). The
  tool-schema conversion, the 64-character function-name limit, and the response model echo
  are written from the documentation. A live call has to confirm them. Decision
  [0090](decisions/0090-google-adapters.md).
- **Bedrock does not name the model** (`bedrock`). The response carries no model id, so the
  model echo check has nothing to compare. The id is checked against your configured set only.
  Decision [0091](decisions/0091-bedrock-converse-adapter.md).
- **Azure Entra ID, sovereign clouds and Foundry project hosts are not supported**
  (`azure-openai`, `azure-openai-responses`). Only the `api-key` header works. Deployments
  that Azure updates automatically, and model-router deployments, cannot be used, because
  each deployment must map to one exact model string. Decision
  [0092](decisions/0092-azure-openai-transport.md).
- **Cohere rerank is not supported.** It bills in search units, which the token ledger
  cannot price, and its reply names no model. Decision
  [0089](decisions/0089-models-of-every-kind-are-routes.md).
- **Together embeddings are not claimed.** Its documented reply has no token count, so every
  call would be billed and then refused. Decision
  [0089](decisions/0089-models-of-every-kind-are-routes.md).
- **Text Embeddings Inference names no model** (`tei`). The model echo cannot be checked.
  Decision [0089](decisions/0089-models-of-every-kind-are-routes.md).
- **Some local models return tool calls as text** (`ollama`, a VERIFIED route). The model
  `qwen2.5-coder:7b` writes its tool call as JSON inside the reply text, and the server
  returns no structured call. kGraf reads that as a finished reply, so a worker on that model
  has nothing to run. Other model packages behave differently.
- **A model name is not a verified model.** A VERIFIED row covers the models named in its
  record. A different model on the same route can behave differently. DeepSeek is known to
  replace an unsupported model name with a cheaper one and return HTTP 200. The model echo
  check exists to catch this.

## References

- Catalog and kind claims: `crates/kgraf-fleet/src/lib.rs` (`CATALOG`, `KIND_CLAIMS`).
- Decisions: [0006](decisions/0006-provider-layer-shape.md),
  [0089](decisions/0089-models-of-every-kind-are-routes.md),
  [0090](decisions/0090-google-adapters.md),
  [0091](decisions/0091-bedrock-converse-adapter.md),
  [0092](decisions/0092-azure-openai-transport.md).
- Provider setup and commands: [USAGE.md](USAGE.md).

## Changelog

- 2026-09-29: first version.
- 2026-09-30: buffered calls scale their time bound with the output cap (see Known limits).
