# Decision 0091: AWS Bedrock is reached through the Converse operations, signed with SigV4, over AWS's own signer and event-stream crates

**Status:** Accepted. The route is unverified: no request has been exchanged with the service.
**Related:** decisions 0006, 0041, 0071, 0080

## Context

Bedrock hosts models from several vendors behind one signed API. Its native per-vendor request
shapes differ, but Converse is uniform. The signing scheme and the binary event framing are exact,
and an error in either fails closed with a 403 or a corrupt stream.

## Decision

1. **One adapter for the Converse protocol.** `kgraf-provider-bedrock` speaks `Converse`
   (`POST /model/{modelId}/converse`) and `ConverseStream` (`/converse-stream`). One adapter serves
   every vendor's model behind Bedrock. The catalog route is `bedrock` (wire `bedrock-converse`,
   confidence `unverified`, additive usage).
2. **Signing and framing use AWS's maintained crates.** `aws-sigv4` signs each request.
   `aws-smithy-eventstream` decodes `application/vnd.amazon.eventstream`, including both CRC
   checks. Both are Apache-2.0.
3. **No credential provider chain.** The AWS SDK's chain reaches the instance metadata service and
   STS or SSO endpoints the user did not configure. A Bedrock API key in
   `AWS_BEARER_TOKEN_BEDROCK` is sent as `Authorization: Bearer <key>` with no signature, and wins
   over access keys. That precedence is kGraf's own rule, not a claim about the AWS SDKs. Otherwise credentials come from `AWS_ACCESS_KEY_ID` and
   `AWS_SECRET_ACCESS_KEY` (with `AWS_SESSION_TOKEN` for temporary keys), then the static keys of
   the profile named by `AWS_PROFILE` (default `default`) in the shared credentials and config
   files. The region is `AWS_REGION`, `AWS_DEFAULT_REGION` or the profile's `region`. A profile
   that needs a network exchange (`role_arn`, `sso_*`, `credential_process`,
   `web_identity_token_file`) is refused with the reason and the command that exports temporary
   keys. Credentials are resolved on every call, the bearer API key, the secret access key and
   session token are registered with the scrubber (0071), and the route's readiness check and the
   built adapter use the same lookup, so
   a missing key or region fails at readiness. The region must match `^[a-z0-9-]+$` because it
   becomes part of the host name, and the secrets are registered when the adapter is built.
4. **Usage is additive.** Converse reports `inputTokens` excluding cache reads and writes, so input
   is `inputTokens + cacheReadInputTokens + cacheWriteInputTokens`. The adapter refuses a
   configuration that says inclusive. `totalTokens` is checked against `inputTokens + outputTokens`
   and that plus both cache counts (the reference does not say which). A total that matches
   neither keeps the answer, because the call was billed: the counts come from their own fields
   and the mismatch is written into the usage's raw blob as `kgraf_note` and logged as a warning.
   Counts that add up to more than a 64-bit total are refused. A `serviceTier` other than `default`,
   or cache writes with a TTL other than `5m`, bill at rates the price table does not hold: the tier
   is recorded in the raw blob, the row is left unreconciled (shown as unpriced, never priced at the
   standard rate), and the answer is returned.
5. **Stop reasons.** `end_turn`, `tool_use`, `max_tokens` and `stop_sequence` map to the core
   reasons. `guardrail_intervened`, `content_filtered`, `malformed_model_output`,
   `malformed_tool_use` and `model_context_window_exceeded` end the turn without an answer and
   raise after the call is counted. Any other value raises.
6. **The response names no model.** Converse echoes neither the requested id nor a substitute, so
   the model-echo assertion has nothing to compare. The id is checked against the configured set
   before the request leaves, and the response carries the requested id.
7. **Cache points and reasoning.** `cachePoint` blocks follow a breakpoint only when the route is
   configured as honoring them. The catalog says no, because the service accepts them for some
   models only, and the flag is not user-settable for the `bedrock` route. A request carrying a reasoning effort is refused, because Converse has no
   model-independent field for it. Reasoning content in a response is kept as reasoning and is
   not replayed. Reasoning tokens are not reported apart, so they count as output.
8. **The stream ends with usage.** `metadata` follows `messageStop` and carries `usage`. A stream
   that ends before it has no usage and stays unreconciled with its request id. An exception frame
   inside a 200 stream raises. A start, delta or response block kind the adapter does not model
   raises rather than being dropped. A corrupt frame late in a chunk does not discard the frames
   decoded before it, so a `metadata` event ahead of the fault still records its usage.
9. **Request text.** The service rejects blank text, so a blank text block is skipped and an empty
   tool result is sent as `(no output)`.
10. **Errors.** An error body is read for at most 15 seconds and capped at 64 KiB; bytes that
    arrived before a timeout or read failure are kept. The `x-amzn-requestid` of an error response
    goes on the row. Redirects are not followed, because the request carries a signature or bearer
    token. A non-finite temperature is refused before sending. A `424`
    (`ModelErrorException`) may follow model work and the reference does not say it is unbilled,
    so its row stays unreconciled. Other 4xx statuses follow the shared rule.

## Options considered

- **The official SDK** with its credential chain: complete, but it reaches endpoints the user did
  not configure and its dependency tree is much larger than the adapter needs. Rejected.
- **A hand-written signer and framer:** a second copy of code AWS maintains and tests against its
  own vectors. Rejected.
- **Per-vendor InvokeModel shapes:** one adapter per model family, with no shared streaming or
  usage. Rejected: Converse already normalizes them.
- **Bearer-token only:** the API key is supported alongside SigV4, not instead of it.

## Consequences

- The route stays `unverified` until a request has been exchanged with the service. Its tests use
  responses copied or constructed from the AWS documentation, each fixture citing its page, and an
  independent SigV4 verifier that recomputes the signature a server would. None of them proves the
  service accepts what the adapter sends.
- Enabling cache points on a model that does not support them makes the service reject the
  request. That is loud. The catalog route leaves them off, and a caller that builds its own
  provider configuration can turn them on.
- The response carries the requested model id, not an echoed one.
- Assumed-role, SSO and credential-process profiles need their temporary keys exported first.
- The base URL carries `{region}`. A private endpoint needs a different base URL, which the
  catalog does not yet let a user override.
