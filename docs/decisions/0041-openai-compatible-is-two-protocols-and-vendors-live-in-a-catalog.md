# Decision 0041: "OpenAI-compatible" is two protocols, and which vendor speaks which lives in a catalog

**Status:** Accepted. Extends decision 0006 and corrects an over-reading of
it.
**Related:** decisions 0006, 0012

---

## Context

### The gap this closes

Decision 0006's stated consequences include: use the Responses API for
OpenAI, not Chat Completions. On at least one recent OpenAI model, Chat
Completions requires reasoning effort set to none for function calling to
work, so it gives tools or reasoning but never both.

That is correct, and it is an argument about OpenAI's own models. It was
implemented as though it applied to the whole OpenAI-compatible ecosystem:
one crate, one surface, a single Responses endpoint.

The consequence went unnoticed until a third-party provider was tried: no
third-party OpenAI-compatible provider implements Responses. It is OpenAI's
own surface. A wide range of other providers and self-hosted inference
servers all serve Chat Completions and only Chat Completions. kGraf could
reach only two vendors, while heterogeneous multi-vendor work is the
project's primary mode. The seam was right; the coverage behind it was two
vendors wide.

The error is worth naming precisely because it is a class, not an incident:
a correct, well-evidenced instruction about one vendor was generalized into
an instruction about a protocol family, and the generalization was invisible
because it read naturally. Mock tests could not catch it, since the mocks
were written to the same wrong assumption: a mock can only confirm that a
schema is parsed the way it was written, not that it matches what a real
server sends.

### Why the usage dialect is configured rather than detected

Chat Completions carries two usage-reporting conventions across the
ecosystem, both inclusive of cached tokens: one nests a cached-token count
inside a details object, the other reports a separate cache-hit and
cache-miss token pair. They differ only in how the split is named.

Reading a hit-and-miss vendor with the details convention finds no details
object, reads zero cached tokens, and attributes the whole prompt to
uncached input. Nothing errors. At current cache discounts, roughly ten to
one, that overstates the input cost of a heavily cached call by close to an
order of magnitude, and a live-cost readout that is confidently wrong is
exactly the failure kGraf's fail-loud principle exists to prevent.

It cannot be inferred from the URL, because the same vendor may change shape
and because the compatible ecosystem is open-ended. So it is configured, and
the hit-and-miss dialect additionally checks itself: it is the only one that
states both halves, so a split not summing to the prompt total raises rather
than being silently reconciled.

### Why a catalog, and why it carries confidence

Adapters must not learn vendor names: provider-specific behavior stays
inside its adapter, and accounting keys on protocol precisely because one
vendor can ship two conventions behind one endpoint. But something must know
which vendor speaks which dialect. Putting that in the adapters would
reintroduce the coupling decision 0006 removed; leaving it to each caller
guarantees someone gets it wrong once.

A separate catalog holding this as data resolves it: a table of vendor, base
URL, wire protocol, capability flags, confidence, and a note, rather than
logic.

Confidence is not decoration. Endpoint research found confirmed instances of
a provider accepting a field and silently ignoring it while still returning
success, and marked several usage shapes as formally unverified. A catalog
presenting all of that with equal authority would launder unverified guesses
into apparent fact. So each entry states how much is known, tests assert
that an unverified entry claims no paid capability, and vendors with
confirmed silent failures carry an explicit warning in their note.

### Why no entry lists model names

Model identifiers move faster than a release. A stale list is worse than no
list: at best it trips a model-echo assertion, and at worst, at the one
vendor confirmed to remap unknown model names to a cheap tier while still
returning success, it silently routes work to the wrong model. The caller
supplies the models it means to use, and the adapter refuses anything
outside that set before a request leaves. A test asserts the catalog names
no model.

### Why local endpoints needed a change

Configuration normally refuses an empty credential, so a forgotten key
fails at configuration time rather than as a confusing authorization error
later. Several self-hosted inference servers take no credential at all, so
that rule made self-hosted models unconfigurable, which sits poorly with a
product that is meant to work locally without cloud dependency.

A separate local-endpoint configuration path resolves it without weakening
the rule, because the absence is now declared: choosing that path states
that no credential is needed, while a forgotten key on a remote endpoint
still raises. The two absences stay distinguishable. A local endpoint is
also sent no authorization header at all rather than an empty one, since
some servers reject a malformed header outright, which would otherwise
present as an authorization failure against a server that has no
authorization at all.

## Decision

The OpenAI family is two wire protocols, and kGraf implements both:

- **Responses** (`POST /responses`, `input`, `max_output_tokens`, usage in
  `input_tokens` / `input_tokens_details.cached_tokens`), OpenAI-proprietary.
- **Chat Completions** (`POST /chat/completions`, `messages`, `max_tokens`,
  usage in `prompt_tokens` / `completion_tokens`), what the third-party
  compatible ecosystem actually serves.

Chat Completions carries two usage dialects, configured per worker because
they cannot be inferred from the URL: a `prompt_tokens_details.cached_tokens`
object, and a `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` pair.

Which vendor speaks which, at which URL, honoring what, lives in one
reviewable data table, with a per-entry confidence. Adapter crates stay
protocol-only and learn no vendor names.

A locally hosted endpoint takes no credential, declared through a distinct
local configuration path rather than inferred from an empty string.

## Consequences

Multiple routes across many vendors plus self-hosting are reachable through
three protocol adapters. Adding a Chat Completions host is now a data row,
not code.

Two routes exist for vendors that serve two surfaces, listed separately
rather than one being chosen silently. Where a vendor's two routes disagree
on the usage convention with the same credential, a test asserts they keep
disagreeing: if they ever match, the catalog has lost the distinction that
makes per-protocol accounting necessary.

Every Chat Completions and Responses entry is inclusive of cached tokens,
and a test enforces it. An additive entry on either wire would undercount
every cached request.

The unverified rows are a work list, not a disclaimer. Each one closes with
a single live call whose raw usage response is recorded.

Cost for a local model derives from the price table like any other, which
for a self-hosted model should say zero. It is not special-cased to free,
because electricity is not free and a user may want it priced.

## Alternatives considered

**Leave Responses as the only OpenAI-family surface.** Rejected: it leaves
every vendor except two unreachable, against the primary heterogeneous-vendor
mode.

**One adapter with a surface flag.** Rejected. The two surfaces differ in
path, request shape, response shape, and usage field names; a flag would
make every method branch and would let a misconfigured flag produce a
plausible wrong number rather than a compile error.

**Drop Responses and use Chat Completions everywhere.** Tempting, since it
reaches everything including OpenAI. Rejected: it would mean OpenAI workers
get tools or reasoning but never both, a capability regression on the vendor
most likely to be doing reasoning-heavy work.

**Detect the usage dialect by probing.** Rejected: a probe costs a real
call, the answer can change, and a wrong guess is silent. Configuration with
a self-checking dialect is cheaper and louder.

**Put the vendor table in the provider crate.** Rejected: it is the
coupling decision 0006 removed. The table lives above the adapters, where it
can name vendors freely because nothing below it can see it.

## Reactivation trigger

A third OpenAI-family surface, a Chat Completions usage dialect neither
existing one covers, or a vendor whose route moves. The first two extend the
usage-dialect representation; the third is a data edit.
