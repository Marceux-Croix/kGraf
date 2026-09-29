# Decision 0071: Transcripts and traces are scrubbed of secrets before they are written, shown, or sent to a model

**Status:** Accepted
**Related:** decisions 0051, 0056, 0057, 0059; kGraf's fail-loud and reuse-first standing
rules; `crates/kgraf-secrets`

## Context

A worker's transcript and its traces can carry whatever a tool printed or a model produced,
unfiltered: reading a credentials file, a network reply with a token echoed in it, a model
quoting something it was told. Left unscrubbed, a credential could end up at rest on disk
and shown verbatim in the application, or sent back to a model on a later turn once it had
already been echoed into the transcript once.

## Decision

**One scrubbing library finds a secret from either of two sources.** Pattern rules recognize
the shape of a known kind of credential (a cloud provider key, a token, a private key, a bare
username-and-password inside a URL). An exact-secret registry holds kGraf's own configured
credentials directly: an API key for a configured model provider, or a value read from an
environment variable for a connected external tool. Either source redacts to a marker naming
only the kind of secret found, never the value itself.

**There are two independent scrubbing passes, not one.** A model-facing pass, using the exact
registry only, runs on anything a running agent is about to be shown, so the agent's own
context is never corrupted by an over-eager pattern match on ordinary code or text. A
persist-and-display pass, using both the registry and the pattern rules, runs on anything
written to disk or shown in the interface. The two intentionally differ: a live agent may
still see a genuine credential that happens to be sitting in a file it was given access to
read, which is a property of giving it file access at all, not something this mechanism is
meant to close.

**Scrubbing runs at one place, before anything else touches the event**, rather than at
several call sites the codebase would have to keep in exact agreement with each other.

**Redactions are counted and reported, never left invisible.** A user sees how many secrets of
which kind were found and removed from a given turn, without ever seeing the value itself.

**Performance stays practical at scale.** The pattern rules are compiled once into a single
combined search, so the common case of "nothing to redact" costs one fast linear scan of the
text.

## Consequences

- **A secret split across two separate pieces of output can still slip through.** A secret
  spread across two different tool calls, or encoded so its shape does not match any pattern,
  is not caught. This is a documented, accepted limit, not a silent gap.
- **A scheduled run's saved instruction is kept exactly as written**, unscrubbed, because
  after a restart kGraf must be able to start that run from the real text; everything the run
  itself produces downstream is scrubbed as usual.
- **A transcript written before this mechanism existed is not retroactively scrubbed.** Only
  what is written from this point forward is covered.
- The scrubbing library is small and self-contained enough to have no networking dependency of
  its own, matching kGraf's local-only design.

## Alternatives considered

- **Scrub only when writing to disk, and let a running agent see the raw text.** Rejected: a
  secret an agent saw once could be echoed back into its own transcript on a later turn,
  reappearing through a different path than the one that first carried it.
- **Port every rule from a full open-source secret-scanning rule set.** Rejected for this
  pass: most of the wider rule set covers vendor-specific tokens outside anything kGraf
  depends on. The table used here is a curated subset, kept additive so a new rule is cheap to
  add later.
- **Use an existing command-line secret scanner as a library.** Rejected: the tool considered
  is built to be run over a checked-out repository from the command line, not called
  in-process on a single string for every tool result, so it would not fit kGraf's call
  pattern without being rewritten anyway.
