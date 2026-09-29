# kgraf-bench-briefs

A per-model brief-following bench (coordination-hardening item 8). It feeds
`kgraf-core`'s `ProfileStore` (`crates/kgraf-core/src/model_profile.rs`),
the seam decision 0075's routing reads: for each `TaskKind`, which model
satisfies the most briefs per dollar spent.

## What it measures

A fixed suite of small, self-contained briefs (`src/brief.rs`), at least two
per `TaskKind` (Interface, Implementation, Tests, Refactor, Scripting, Docs),
each with a tiny fixture repository under `fixtures/`. For each configured
route and brief, one worker runs through kGraf's normal worker path
(`kgraf_host::worker`: the same assembly and `run_segment` `kgraf worker`
uses, a real git repository, a real worktree, the real tool set, the real
ledger) against a fresh copy of the fixture. Whatever it commits is checked
**mechanically**: a `cargo test` run that must pass, a script whose stdout
and exit status must match exactly, or a doc file whose required facts and
word cap the check reads directly. There is no model judge anywhere in this
crate; a judge model would just be a second guess, not a measurement.

Every priced attempt's `(model, vendor, kind, satisfied, cost)` is recorded
into the `ProfileStore` at `--state-dir` with `Source::Bench` (see "Prices"
below for what happens when a model has no price), and the run prints a
per-model, per-kind table (attempts, satisfied, rate, cost per satisfied)
marked `bench` and carrying the suite's hash (`brief::suite_hash()`), so a
result is always traceable to the exact briefs that produced it.

## What it does not measure

These are small, single-file tasks in a fresh repository with no MCP
servers, indexed as a real run indexes a repository: a prior on a
model's brief-following, not a repo-scale benchmark, and not a substitute for
watching the model work a real task. Per decision 0075's routing rule, a
bench tally (`Source::Bench`) is a prior; a live, verified tally
(`Source::Live`) for the same model and kind, once one exists, overrides it.

## Running it

```
kgraf-bench-briefs \
  --route kimi:kimi-k2 \
  --route deepseek:deepseek-chat \
  --state-dir .kgraf-bench
```

`--route` is `route:model`, repeatable; the route id is `kgraf-fleet`'s
catalog id (`kgraf providers`). Other flags: `--brief <substring>`
(repeatable, runs only matching briefs; default: every brief), `--reps <n>`
(default 1), `--dry-run` (lists planned attempts and an estimated cost where
the model is priced, "unpriced" otherwise; makes no provider call and writes
nothing).

Each attempt gets the budget `kgraf worker` gives a worker by default: 12
model rounds, a 4,096-token output ceiling per round and a 128,000-token
context window. The values come from the same constants the `kgraf worker`
flags read (`kgraf_host::worker::config`), so the bench and the command
cannot drift apart. `--budget <rounds>`, `--max-tokens <n>` and
`--window <tokens>` override them, as the `kgraf worker` flags of the same
names do. The dry-run estimate is the rounds times the output ceiling,
priced at the output rate.

No route is ever refused for price, and this bench sets no spend cap of its
own; a spend limit belongs in the provider's own console, not here.

## Prices

The default price source is kGraf's own vendored litellm catalog
(`kgraf_core::litellm::LiteLlmCatalog`, loaded from
`third_party/litellm/model_prices_and_context_window.json`) -- there is no
separate price file to hand-maintain for a route already in
`kgraf providers`. `--prices <path>` is optional and only overrides or adds
entries (the same JSON shape `kgraf orchestrate --prices` reads): use it for
a model the vendored table does not have or gets wrong, e.g. a self-hosted
one.

Prices are looked up by the exact model id given in `--route`. When the
table has no entry for it, the bench prints a warning before any call,
naming the closest catalog keys (a dated suffix or a missing provider prefix
is a common near miss); it never picks one of them. Price the model with
`--prices` if one of them is the same model.

A model with no price entry still runs. Its cost is never recorded as a
fabricated `$0`: it prints as "unpriced" and is left out of the
`ProfileStore` tally's cost sum entirely, rather than making it look free to
decision 0075's router. A local route (`ollama`, `llama-cpp`, or any route
`kgraf-fleet` builds with no credential and a localhost base URL) with no
price prints "local, unpriced" instead of plain "unpriced" -- decision 0041
holds a local model is not special-cased to free, so this is informational,
not a claim about its real cost. A local route the price table *does* cover
is costed exactly like a hosted one.
