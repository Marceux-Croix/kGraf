# Decision 0092: Azure OpenAI is a transport option of the two OpenAI adapters, with a configured deployment-to-model mapping

**Status:** Accepted. The routes are unverified: no request has been exchanged with the service.
**Related:** decisions 0006, 0041, 0071, 0080, 0090, 0091

## Context

Azure hosts OpenAI models behind per-resource endpoints, and a deployment is a named, versioned
instance the account owner creates. The body is OpenAI's, which is why the parser is shared. The
differences are all at the edges: where the request goes, how it authenticates, what "model"
means, and a content filter that can cut a completion or block a prompt.

## Decision

**1. A transport option, not a second parser.** Azure OpenAI serves the Chat Completions and
Responses bodies unchanged. `kgraf-provider-openai` gains `AzureTransport`, given to
`ChatCompletionsAdapter::with_azure` and `OpenAiAdapter::with_azure`. It changes the address, the
credential header, the model check and the content-filter handling. Body building, usage
normalization, streaming and tool handling are the existing code (the same shape as the Anthropic
adapter's per-model URL transport, decision 0090). Two catalog routes, both Confidence
`unverified`: `azure-openai` (wire `azure-openai-chat`, Chat Completions) and
`azure-openai-responses` (wire `azure-openai-responses`, Responses). The routes report their own
adapter names to the ledger; usage is inclusive with `prompt_tokens_details.cached_tokens` and
`completion_tokens_details.reasoning_tokens` (Responses:
`input_tokens_details.cached_tokens` and `output_tokens_details.reasoning_tokens`), as on OpenAI.

**2. Two surfaces, chosen by the API version.** `KGRAF_AZURE_OPENAI_API_VERSION` is `v1` or a
dated version (`YYYY-MM-DD`, optionally `-preview`).

| Body | `v1` | dated version |
|---|---|---|
| Chat Completions | `https://{resource}.openai.azure.com/openai/v1/chat/completions`, deployment as `model` in the body | `.../openai/deployments/{deployment}/chat/completions?api-version={version}`, no `model` in the body |
| Responses | `.../openai/v1/responses`, deployment as `model` | refused when the route is built |

Sources: Microsoft Learn, "Azure OpenAI in Microsoft Foundry Models REST API reference"
(`/azure/ai-services/openai/reference`), "Azure OpenAI API version lifecycle"
(`/azure/ai-foundry/openai/api-version-lifecycle`, the v1 API) and "Responses API"
(`/azure/ai-foundry/openai/how-to/responses`). The Responses body is shown only at
`/openai/v1/responses`; no page shows a dated Responses path, so the Responses route accepts
`v1` alone rather than post to an undocumented path.

**Version floor.** The lifecycle page lists `stream_options.include_usage`,
`completion_tokens_details.reasoning_tokens` and `max_completion_tokens` as added in
`2024-09-01-preview`. The Chat route streams with usage, reads reasoning tokens and sends
`max_completion_tokens`, so a dated version before `2024-09-01` is refused when the route is
built, at the credential step and again when the transport is constructed. It would fail on the
wire or return no usage. A date without `-preview` must be `2024-10-21` (the first GA version in
the REST reference) or later, since none is published between the floor and it. Above those
bounds the check cannot tell a published version from an unpublished one: it is a floor, not a
list, and a version that does not exist fails on the wire.

**3. Credential: the `api-key` header.** Settings come from `KGRAF_AZURE_OPENAI_API_KEY`,
`KGRAF_AZURE_OPENAI_RESOURCE`, `KGRAF_AZURE_OPENAI_API_VERSION` and
`KGRAF_AZURE_OPENAI_DEPLOYMENTS`, all required. The four travel through the fleet's one-string
credential interface (as the Vertex settings do), and the key alone is registered with the
scrubber (0071); the header value is marked sensitive. **Entra ID tokens are not supported.**
Microsoft's Rust crate for them (`azure_identity`) pulls in the Azure SDK core, which is not a
small dependency for one header; a bearer source can be added as a second auth mode later.

**4. Resource and version are validated before they reach a hostname or a query.** The resource
is a bare DNS label (2 to 64 letters, digits and hyphens, none at either end), lower-cased, and
the host suffix is fixed at `openai.azure.com`: a value with a dot, a slash, a colon, a query or
whitespace is refused, so it cannot steer a request to another host. The version is `v1` or a real
calendar date with the documented shape. Deployment names are limited to letters, digits, `-`,
`_` and `.` because they reach a URL path, and a name made of dots alone (`.`, `..`) is refused
because it is a path segment that steps up or stays in place. Sovereign-cloud hosts
(`openai.azure.us`, `openai.azure.cn`) and Foundry project hosts (`*.services.ai.azure.com`) are
therefore not supported; a Foundry host is named as unsupported instead of as a bad URL. Each
failure names its variable and never the key.

**5. The deployment stands in for the model, and the mapping is checked.** A request names a
deployment. The response `model` names the underlying model, so the configuration maps each
deployment to it (`deployment=model,deployment=model`) and the model-echo check compares the
response to the mapped model, in `complete`, in every streamed chunk, and in the Responses events.
A deployment listed for the adapter with no mapping fails when the adapter is built. A response
naming any other model, the deployment name included, raises `ModelSubstituted` with the call's
spend kept on an unreconciled row. The mapped string must equal what the service returns, dated
suffix included; the error shows both strings, so the first mismatch tells the user what to set.
Microsoft's Responses page shows no response `model` field, so what `/openai/v1/responses`
returns there (the mapped model, or the deployment name) is not settled. The check stays strict
for that reason: if the service returns the deployment name, the call raises `ModelSubstituted`
with both strings shown, rather than accept an unchecked answer.
The alternative of accepting any model that starts with the mapped name was rejected: it would
accept a substituted snapshot silently.

**6. The content filter settles the call and raises `TurnNotUsable`.** The call billed, so its
usage is recorded first. Then any of these raises with the stop reason shown: a choice with
`finish_reason: "content_filter"` (`content_filter`); a `filtered: true` anywhere in
`prompt_filter_results` or in a choice's `content_filter_results` (`prompt_filter_results` for the
first, `content_filter` for the second); a Responses `content_filters` entry (Microsoft Learn, "Responses API": `blocked`,
`source_type`, `content_filter_results`) with `blocked: true`, or with no `blocked` field and a
category `filtered: true`, or an incomplete response whose reason is `content_filter`
(`content_filter`); an explicit `blocked: false` is respected. In a stream the same
fields are read from every chunk or terminal event and the error is raised after the usage chunk
settles the call. A prompt blocked outright is an HTTP 400 with code `content_filter`; that call
billed nothing, so it stays an HTTP error and closes as failed. Only the Azure transport raises on
`content_filter`; on other Chat Completions routes the stop reason stays `Other`. A
`content_filter_results.error` (the filter did not run, so the content went out unchecked; the
content-filter page tells callers to verify there is no such object) settles the call and raises
with the stop reason `content_filter_results_error`, on Chat Completions and in a Responses
`content_filters` entry alike. A stream the filter cut may end with no usage
chunk; it then raises the filter reason rather than a truncation, and the call stays open for
reconciliation, since nothing was counted. A stream that ends early after a chunk that only
reported a filter error is a truncation: that reason says nothing about why the stream ended. Deltas delivered before the filter acted have already
reached the caller's sink, as with any stream; the final result is the error.

**7. Stream openings.** Azure may open a stream with a chunk that has `prompt_filter_results`, an
empty `id` and an empty `model`, and no choices. On the Azure transport an empty `model` is not
checked (a later chunk must still name the mapped model, or the stream raises), and the request id
is the first non-empty chunk `id`. The plain adapter still treats an empty `model` as a
substitution. This opening is not stated on the cited pages, so the tolerance is recorded as
unverified with the route.

**8. Priced only through a route-scoped key.** An Azure request names a deployment, and a
deployment name is the account owner's choice: one called `gpt-4o` can serve another model than a
bare `gpt-4o` price describes. So an Azure route never falls back to a bare model price
(`PriceTable::require_route_scoped`, applied to every price table the host, the bench and the
command line build: `kgraf spend --prices`, `kgraf schedule list --prices` and the run-spend
label read the file through the host's reader).
A price is found only under `<deployment>@<route>`. The bench builds that key from the verified
deployment mapping: the deployment's mapped model is looked up as LiteLLM's `azure/<model>` key,
and when the vendored table has it the deployment is priced from it, else the deployment stays
unpriced. That entry is the standard deployment rate: a data-zone, regional or provisioned
deployment bills differently and the mapping does not say which a deployment is, so the bench
says so and a user price for `<deployment>@azure-openai` is the way to price one of those. The
bench looks prices up by route as the ledger does, and no price problem aborts an attempt. A user price file prices a deployment with the same `<deployment>@azure-openai` key.
Cost shows as unpriced, never zero and never refused.

**9. The output limit is `max_completion_tokens`.** Reasoning deployments refuse `max_tokens`, and
every dated version the transport accepts (2024-09-01-preview and later) takes
`max_completion_tokens` for all deployments, so the Azure Chat transport sends only that field.

**10. Rate limits are keyed per deployment.** Azure limits apply per deployment. The adapters
record observed limits and any 429 under the resource origin plus the deployment
(`kgraf_provider::limits::scoped_key`), and the host reads them under the same key
(`kgraf_fleet::limit_key`). Azure headers are read with the Azure rules: the reset header is a
bare number (read as seconds; the quota page gives `10` and `300` with no unit), and a 429's
`retry-after-ms` (documented in milliseconds), or `retry-after` in seconds, is the wait the
service asks for. The reset value is recorded and nothing waits on it: retry timing belongs to
the retry policy. A recorded observation merges by field, so a 429 that carries only
`retry-after` keeps the limits an earlier response reported. A route with several deployments
reports the tightest limit over their keys, and the key with the newest 429, so a 429 on any
one deployment halves the route's learned rate. The same rule holds for every route
whose catalog URL has a placeholder (`{resource}`, `{region}`, `{project}`): the host looks under
the URL the adapter holds after the settings resolve, not the catalog text, which never matched.
Bedrock's adapter holds its `{region}` URL unresolved and records under it, so its key is that
configured URL (one region per process), returned without reading any credential. A route whose settings do not resolve reports no limit.

**11. Deployments that cannot be used.** A deployment Azure updates automatically to newer model
versions, and a model-router deployment, answer with a model name that changes or differs per
request. The check compares against one configured string, so no mapping is right for them. They
are not usable on these routes; deploy a fixed model version and map it.

## Options considered

- **A separate `kgraf-provider-azure` crate:** a second copy of the body builder, usage
  normalizer and stream parser, which is what the transport-option pattern was chosen to avoid.
  Rejected.
- **The Azure SDK crates:** cover Entra ID and the sovereign clouds, but the dependency tree is
  large for one header. Rejected for now.
- **Trust the response `model` without a mapping:** the response names a model the user never
  configured, so the check would compare nothing. Rejected under fail loud.
- **Infer the mapping from the deployment name:** deployment names are free text. Rejected.

## Consequences

- Both routes stay `unverified` until a request has been exchanged with the service. Their tests
  use bodies copied or constructed from Microsoft's documentation, each fixture citing its page
  (`crates/kgraf-provider-openai/tests/fixtures/azure/`). None proves the service accepts what the
  adapter sends.
- The Chat route sends `max_completion_tokens`, so reasoning deployments are addressed the same
  way as others; that is from the lifecycle page and unverified against the service.
- The catalog does not set `refuses_temperature_with_reasoning` for either route: no measurement
  says the service rejects it, and the catalog reserves that flag for measured routes.
- Entra ID, sovereign clouds and per-resource private endpoints need a later decision.

## Amendment (2026-09-29): embeddings through the same transport

Supersedes the text above where they differ.

- **The embeddings body is a third transport kind.** `AzureTransport::embeddings` serves
  OpenAI's embeddings body for the embedding kind (decision 0089), given to
  `OpenAiEmbeddings::with_azure`. Both catalog routes, `azure-openai` and
  `azure-openai-responses`, claim the embedding kind; the ledger adapter name is
  `azure-openai-embeddings`. Unverified, as the other Azure routes.

  | Body | `v1` | dated version |
  |---|---|---|
  | Embeddings | `.../openai/v1/embeddings`, deployment as `model` in the body | `.../openai/deployments/{deployment}/embeddings?api-version={version}`, no `model` in the body |

  Sources: Microsoft Learn, "How to generate embeddings with Azure OpenAI"
  (<https://learn.microsoft.com/en-us/azure/ai-foundry/openai/how-to/embeddings>), and the
  2024-10-21 inference specification (`specification/cognitiveservices/data-plane/
  OpenAIInference/stable/2024-10-21/inference.json` in `Azure/azure-rest-api-specs`), whose
  reply schema for `/deployments/{deployment-id}/embeddings` requires `object`, `model`,
  `data` and `usage.{prompt_tokens, total_tokens}`. The specification's own example body
  omits `model`; the adapter follows the schema and raises on a reply without it, which is
  the fail-loud reading until a live call settles it.
- **No version floor for embeddings.** The chat floor (`2024-09-01-preview`) exists for stream
  usage and `max_completion_tokens`, neither of which the embeddings body uses; any dated
  version of the documented shape is accepted.
- **The same key, header, mapping and check.** `api-key`, the deployment mapping
  (`KGRAF_AZURE_OPENAI_DEPLOYMENTS`), and the reply's `model` checked against the model the
  deployment is mapped to. A deployment without a mapping is refused when the route is built.
  Only the API key is registered with the scrubber, as for the generative routes.
- **Bounds.** At most 2,048 inputs and 300,000 tokens across them per request (the embeddings
  page's best practices); a larger input is split into requests within them.
- **Limits and prices.** Headers are read with the Azure rules and recorded under the
  deployment's key (`AzureSettings::limit_key`), as for chat. A price is looked up only under
  `<deployment>@<route>`; the ledger row names its catalog route whether or not a rate limiter
  is configured, so a bare model entry never prices it.
- Rerank is not served through Azure: no rerank endpoint is documented there.
