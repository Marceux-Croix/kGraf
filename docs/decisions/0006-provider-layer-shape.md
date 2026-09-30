# Decision 0006: Hand-rolled provider clients, a thin core trait with opt-in capabilities, and per-protocol accounting

**Status:** Accepted
**Related:** decisions 0001, 0005

## Context

### Why hand-rolled

There is no official Anthropic Rust SDK. Every Anthropic crate on crates.io
is unofficial and mostly stale.

The three credible cross-provider crates surveyed each corrupt exactly the
data the accounting design depends on:

- One crate models the Anthropic surface well but its cross-provider layer
  **drops the cache TTL split**, so cost cannot be reconstructed. It has had
  many published versions with several breaking-capable minors in a short
  span, its request type is crate-private so a raw request can never be
  constructed, and its own audit found several cache and usage-mapping bugs
  in one pass.
- Another keeps both TTL buckets but **concatenates Anthropic text blocks
  into one string**, destroying the boundaries needed for cache breakpoint
  placement.
- A third **sums cache writes and reads into one number**, a large error.

The decisive property is failure mode: **a stale crate fails silently.** It
parses the response, drops the new field, and hands back a plausible wrong
number. For a product promising a live cost readout, that is worse than a
compile error.

The hand-rolled path was proven in a working prototype at roughly 90 lines
of serde, with byte-identical round-tripping, surviving signatures, and a
demonstrated cost difference between honoring and dropping the TTL split.

### Why a thin trait plus capability traits

A six-method mandatory interface plus a larger set of optional capability
interfaces, discovered by type assertion, is the pattern that makes a
heterogeneous provider fleet tractable without lowest-common-denominator
design. Capability as a separate small interface lets one provider support
cache control, cancellation, or streaming without forcing every other
provider to fake it.

The counterexamples in surveyed reference systems are instructive: a single
untyped method returning a bare dict with zero runtime enforcement is too
thin to carry usage, cache counts, cancellation, streaming, or cost; and
declaring one method while hiding two more as undeclared duck-typed extras
is a language-specific workaround that does not translate to Rust.

### Why accounting normalizes per protocol

Providers split cleanly into two incompatible conventions.

**Additive:** input excludes cache reads and writes, so the true total is
the sum of all three. Anthropic and Kimi's Anthropic-compatible endpoint use
this convention.

**Inclusive:** cache counts are carved out of the reported input total.
Every other surveyed provider uses this convention.

Adding the cache fields on an inclusive provider double counts. Failing to
add them on an additive provider undercounts.

Kimi ships both conventions behind one API key. The same credential against
its native path and its Anthropic-compatible path gives opposite semantics.
Keying cost logic on provider name rather than protocol silently corrupts
every cached request.

### Why the model echo assertion

Three confirmed instances of the same silent-failure class, all returning
HTTP 200:

- **DeepSeek silently remaps unknown model names to its cheap model.** A
  typo'd or stale model string returns a 200 from the wrong model with no
  error. That is a correctness bug in a worker pool, not only a billing one.
- **Google's OpenAI-compatibility layer ignores unsupported parameters**
  rather than rejecting them.
- **xAI has no cache-control field in its schema at all**, so cache
  breakpoints are dropped without comment.

## Decision

Provider clients are hand-rolled with `reqwest` and `serde`. No unified
LLM-client crate is used for Anthropic or Kimi.

The seam is a small mandatory trait plus separate opt-in capability traits,
never a wide interface with capability booleans. Provider types never
escape the adapter crate.

Usage rides the normalized event stream on every event, not a side channel.

Token accounting normalizes per protocol, not per provider.

Every adapter asserts that the model in the response is the model it
requested.

## Consequences

kGraf owns HTTP, retries, backoff, streaming assembly, cancellation, and
rate-limit handling for every provider, rather than delegating any of it to
provider SDKs.

**Per-provider capability flags default to false**, specifically for cache
control. Emitting cache breakpoints at a provider that drops them costs
money silently and gains nothing.

**Three usage adapters minimum**: Anthropic-native, OpenAI-details, and
DeepSeek hit-and-miss. Within the Anthropic-native family a further flag is
needed for whether input includes or excludes cache reads.

**Persist the raw usage blob verbatim on every call.** Several protocols
changed shape or added fields within the last year, and the raw blob is the
only way to re-derive cost after a pricing or schema change.

**The stored normal form is six independent fields**, never derived at
write time: uncached input, cache read, cache write, text output, reasoning
output, and the raw blob.

**Streaming needs a reconciliation path for dropped streams.** OpenAI
states explicitly that an interrupted stream may never deliver its usage
chunk. A request that billed but was not counted is a defect, so
unreconciled streams must be surfaced rather than dropped.

**Anthropic streaming usage must be merged field by field across two
events, and the delta is cumulative**, so take the last and never sum. A
common belief that the delta carries only output tokens is true of legacy
examples and false of the current schema.

**Per-agent model selection and sticky fallback chains** are first-class. A
fallback engaged within a run stays engaged, and a nested run inherits it
rather than rediscovering that a provider is down.

**Cache control is first-class in the trait from the first commit**, with
write and read counted separately. This is where the cost promise is
actually kept.

**Do not rely on the tool-result error flag** if DeepSeek is in the pool; it
is ignored there, making a failed tool call indistinguishable from a
successful one. Encode failure in the content as well.

**Treat xAI as an OpenAI-Responses provider**, not an Anthropic one. Its
Anthropic-compatible route is documented as deprecated, and it contradicts
itself on reasoning attribution within its own reference.

**Use the Responses API for OpenAI, not Chat Completions.** On GPT-6, Chat
Completions requires reasoning effort set to none for function calling to
work, so it gives tools or reasoning but never both.

## Alternatives considered

**A unified cross-provider layer such as LiteLLM.** Rejected: it would
require a Python runtime inside a Rust-core desktop application, and it was
demonstrated to silently delete cache control for any model outside its
allowlist, which is the same failure class the other surveyed crates show.

**A cross-provider Rust crate as the client layer.** Rejected on the TTL
loss, the churn rate, the private request type, and the audit findings
described above.

**OpenRouter as the single upstream.** Rejected as the default: it adds a
purchase fee, its sticky routing expires after a short window and keys on a
hash of the opening messages unless an explicit session id is passed, and it
reproduces a stale minimum-cacheable-prefix table. Retained as a fallback
and breadth option, since it genuinely honors cache control including a
one-hour TTL and takes no markup on inference.

**Hand-rolled clients behind a thin trait.** Chosen.

**Amended 2026-09-29: an inconsistent usage object keeps the answer.** When a hybrid endpoint's
usage object cannot be trusted (`cached_tokens` above the cache read the adapter reads, or
`total_tokens` not equal to the parts), the Anthropic adapter no longer refuses the reply after
the call was billed. It returns the answer, settles nothing, leaves the ledger row
unreconciled with the counted fields as a lower bound and a `kgraf_note` in the raw usage, and
logs a warning. The row is then priced by the reconcile step (`kgraf unreconciled resolve`), not
guessed. The direct `normalize_usage` function still refuses such an object.
