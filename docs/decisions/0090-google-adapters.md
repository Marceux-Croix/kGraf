# Decision 0090: Google routes are a native Gemini adapter, Gemini on Vertex AI, and Anthropic on Vertex AI

**Status:** Accepted
**Related:** decisions 0006, 0041

## Context

The existing `google` route reaches Gemini through the OpenAI-compatibility layer, which
ignores parameters it does not support. Thinking control, thought summaries and cached-token
accounting are not reliably available there. Vertex AI serves Gemini and Claude behind one OAuth
scheme, one project and one region. None of the three routes below has been run against the live
service; their tests use bodies written from Google's published references, and the catalog marks
each one unverified.

## Decision

1. **Three routes, two adapters.**

   | Route | Wire | Adapter | Auth | Usage |
   |---|---|---|---|---|
   | `gemini` | `generateContent`, `streamGenerateContent?alt=sse` | `GeminiAdapter` | API key in `x-goog-api-key` | inclusive |
   | `vertex-gemini` | the same body under `.../publishers/google/models/{model}` | the same adapter | OAuth bearer token | inclusive |
   | `vertex-anthropic` | the Anthropic Messages body at `.../publishers/anthropic/models/{model}:rawPredict` and `:streamRawPredict` | the Anthropic adapter with `ModelInUrlTransport` | OAuth bearer token | additive |

   `gemini` reads `KGRAF_GOOGLE_API_KEY`, the same key the `google` route reads.

2. **Claude on Vertex reuses the Anthropic adapter.** The body, streaming events, usage object and
   stop reasons are the Messages shapes. The URL carries the model and a verb, the body carries
   `anthropic_version: vertex-2023-10-16` instead of `model`, the credential is a bearer token, and
   the response may spell a versioned model `name-version` where the path spells it
   `name@version`. The transport option covers those four differences and names no vendor, and
   errors raised on this route carry the transport's adapter name. Google's guide does not say how
   the response model relates to the id in the path, so the echo accepts three forms and refuses
   every other difference: the id as sent; `name@version` answered as `name-version`; and a bare
   `name` answered as `name-` plus an eight-digit date.

3. **Gemini usage.** `promptTokenCount` contains `cachedContentTokenCount`, so the cached share is
   carved out of the input; there is no cache write. `toolUsePromptTokenCount` joins the input.
   `candidatesTokenCount` is text output and `thoughtsTokenCount` is reasoning output, counted
   apart. `totalTokenCount` is cross-checked: a total above the counted parts is billed as further
   reasoning, and a total below them is refused, leaving the call unreconciled with the counted
   fields as a floor. Only `promptTokenCount` proves usage arrived, because the service omits
   zero counts. A stream restates cumulative totals, so reports are merged field by field: a later
   value replaces an earlier one, a field only an earlier report carried stays, and nothing is
   summed. The exception is `totalTokenCount`: a total from an earlier report is dropped when a
   newer report omits it, and is never checked against counts it no longer matches.

4. **Finish reasons are enumerated.** `STOP` ends the turn, or is a tool use when the turn holds a
   function call. `MAX_TOKENS` is the output ceiling. Blocks, refusals, escalations, policy limits
   and malformed calls settle
   the call and raise a not-usable-turn error with the reason; a blocked prompt does the same. Any
   other value, including `FINISH_REASON_UNSPECIFIED`, settles the call and raises a decode error
   naming it. A stream that ends without a reason is truncated.

5. **Function calling.** Tools are `functionDeclarations` in the dialect each host documents.
   The Developer API takes `parametersJsonSchema` (full JSON Schema), names up to 128 characters
   and an optional call `id`. Vertex's function-calling guide documents only `parameters`, an
   OpenAPI subset, names up to 64 characters and no `id`, so the schema is converted there, no id
   is sent, and a schema keyword the subset cannot express is refused rather than dropped. (The
   v1 REST reference also lists `parametersJsonSchema` on a Vertex declaration; the guide does
   not, so the adapter keeps to `parameters` until a live call shows the other is accepted.) The
   conversion follows the `Schema` resource of the v1 reference: it folds `["t", "null"]` and
   `anyOf: [X, {"type": "null"}]` into `nullable`, and passes the keywords that resource lists
   (`format`, `title`, `default`, `example`, `pattern` and the numeric, length and item-count
   bounds). Three more rules keep out a schema the service answers with a 400. A node with no
   `type` gets one only where it is implied (an `enum` of strings is a string), and otherwise is
   refused. `enum` is a non-empty list of strings, so an empty list or any other value is
   refused. An object with no properties, which is what a free-form object is, is refused. Each
   refusal is raised before any request and names the tool and the node's path. A function name
   must start with a letter or an underscore and hold only letters, digits, underscores, dots,
   colons and dashes; this is checked for the declared tools and for the calls in the history. A
   name over the host's limit is refused before sending. A `-latest` model alias is refused
   before sending, because the service answers under a concrete id that the echo check would
   refuse after the call was billed. A function response names the function, so the adapter resolves
   the name from the call earlier in the history; a result with no matching call is refused
   before sending. Results are sent under `output`, or `error` when the tool failed.

   Thought signatures travel with the history: the signature is stored on the tool-use block and
   its event, and rebuilt on resume, so a resumed run or a new worker process sends it back
   unchanged. Older transcripts still read. A signature is valid only for the model that made it,
   so it carries a producer tag, the adapter and the model asked for, and is sent only to the
   adapter and model whose tag it holds. A call whose signature is absent, untagged or tagged for
   another model gets Google's documented `skip_thought_signature_validator`, only on the first
   call of a step and only on a `gemini-3` model; on any other model nothing is sent.

   A signature on a text part is kept the same way, on the text block and on the round's settled
   message. A signature that follows the text, including one on an empty text part, joins the
   last text block; a signed part is never merged with an unsigned one; a block that carries only
   a token is sent back as an empty text part with its signature. Thought summaries are not kept
   with their signatures: the history has no place for a signed summary, and the service
   validates only a function call's token.

6. **Thinking.** A reasoning effort maps to a token budget (1,024, 8,192, 24,576) for `gemini-2.5`
   models and a level for `gemini-3` models. A family that is neither is refused, and a level a
   model does not document is refused rather than substituted (`gemini-3-pro` takes `low` and
   `high` only; the table follows the Vertex thinking guide, including the Flash-Lite and image
   models). A budget is capped at half of the output ceiling, because thinking tokens count
   against it, and a budget under the model's documented minimum (512 on `gemini-2.5-flash-lite`,
   128 elsewhere) is refused. `includeThoughts` is set so summaries arrive as reasoning blocks. No thinking
   config is sent without an effort.

7. **Vertex credentials and settings.** OAuth is the `gcp_auth` crate (MIT): service-account key
   files, the gcloud user credentials file, the metadata server, the gcloud command, and token
   caching and refresh. The crate's own discovery probes the metadata server and runs `gcloud`,
   which is network and process activity the user did not ask for, so the adapter builds the
   provider for the one source named and nothing else. Settings come from the environment when
   the route's credential is resolved, so a missing value fails before any request. There is no
   default region.

   | Setting | Variables, first set wins |
   |---|---|
   | Credentials | `KGRAF_VERTEX_CREDENTIALS` (`adc`, `metadata`, `gcloud`, or a key file path), then `GOOGLE_APPLICATION_CREDENTIALS` |
   | Project | `KGRAF_VERTEX_PROJECT`, `GOOGLE_CLOUD_PROJECT`, `GCLOUD_PROJECT`, then the key file's `project_id` |
   | Region | `KGRAF_VERTEX_REGION`, `GOOGLE_CLOUD_LOCATION`, `GOOGLE_CLOUD_REGION` |

   `adc` is the service-account key file `GOOGLE_APPLICATION_CREDENTIALS` names if set, else the
   gcloud user credentials file; it never reaches the metadata server or runs `gcloud`. `metadata`
   and `gcloud` are used only when named. Other file types, such as `external_account` files, are
   not supported. The hosts are `{region}-aiplatform.googleapis.com`, `aiplatform.googleapis.com`
   for `global`, and `aiplatform.us.rep.googleapis.com` and `aiplatform.eu.rep.googleapis.com` for
   the `us` and `eu` multi-regions.

   Project and region are validated before they reach a hostname or path. A key file is parsed when
   the adapter is built; the other sources are initialized on the first request. A token failure
   sends nothing and closes the call as unbilled. Tokens are never logged.

   The credential files are secrets a worker's shell can read. When the route is built, the
   values in the file the source reads (a service account's private key and key id, a user
   credential's refresh token and client secret) are registered with the scrubber before the
   adapter is built. A key spans lines and a file spells each break as `\n`, so the key is
   registered as decoded, as the file spells it, and as each of its body lines.

   A user credential is billed to the project of the OAuth client that minted it unless a request
   names another, and Vertex AI is usually not enabled there. The user file records the project
   `gcloud` was told to use (`quota_project_id`), and both Vertex Gemini and Vertex Claude requests
   send it as `x-goog-user-project`. A service account, the metadata server and the gcloud command
   send no such header.

8. **The echoed model.** `modelVersion` must equal the requested id, or the requested id plus a
   numeric version suffix, because Vertex resolves an alias such as `gemini-2.0-flash` to
   `gemini-2.0-flash-001`. Any other difference, such as `-lite`, is a different model and is
   refused after the call is settled.

## Options considered

- Keeping thought signatures in an adapter-local cache: replaced, because a resumed run and a new
  worker process each build a new adapter, so the cache was empty when a history needed it.
- Extending the `google` route's OpenAI Chat adapter: rejected, because the compatibility layer
  ignores parameters and the usage arithmetic belongs to the native shape.
- A separate Vertex Claude parser: rejected, because it would duplicate the direct route's
  streaming, tool and usage code.
- `google-cloud-auth` (Apache-2.0): not chosen, because its default features pull a native TLS
  stack and a second `reqwest`. `yup-oauth2` is a device-flow library first.
- A default region: rejected, because the region sets where data is processed and which models
  exist.

## Consequences

- The three routes appear as unverified in `kgraf providers`. The first live call on each should
  check the recorded raw usage object, the model echo, and for Gemini 3 the thought-signature round
  trip. A wrong rule fails loud and leaves the call unreconciled with its usage.
- Cache control is not claimed on any of the three.
- A model with no entry in the price table reads as unpriced, never as zero.
