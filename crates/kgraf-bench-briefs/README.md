# kgraf-bench-briefs

A per-model brief-following bench. It feeds
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
**mechanically**: a `cargo test` run that must pass and run at least the
fixture's tests, a script whose stdout and exit status must match exactly, or
a doc file whose required facts and word cap the check reads directly. There
is no model judge in this crate.

The check is strict:

- A named test must have run, and a `tests` brief must fail on deliberately broken copies of the code.
- A commit that changes a protected file (a test or source the brief says not to touch) does not count.
- The worker's code runs in `kgraf-sandbox` with the network off and a time limit. A tree with a `build.rs`, a `.cargo` directory or a toolchain pin is refused.
- Nothing a test prints is read. Each test runs alone and is judged by its exit status.
- Before the first model call, the runner checks a known-good crate. If that fails, the run stops: a broken toolchain is not a set of failed briefs.

`src/check.rs` documents each rule in full.

A provider or harness error (an outage, a failed ledger write, a worker that
could not run, a check that could not run) is printed as `ERROR (not tallied)` and never recorded: only an
attempt that measured the model is a tally.

Tallies are kept per suite version. The store at `--state-dir` holds the
current suite's bench tallies, and `bench_suite.json` beside it names the
suite. When the suite hash changes, the first recorded attempt copies the old
store to `model_profiles.<hash>.json` and starts the bench tallies from zero
(live tallies stay). The printed table names any set-aside suite instead of
mixing it in.

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
nothing). `--json-out <path>` writes every attempt's full record, with token counts and
wall time, as a JSON array (in a dry run too).

Each attempt gets the budget `kgraf worker` gives a worker by default: 12
model rounds, a 16,384-token output ceiling per round and a 128,000-token
context window. The values come from the same constants the `kgraf worker`
flags read (`kgraf_host::worker::config`), so the bench and the command
cannot drift apart. `--budget <rounds>`, `--max-tokens <n>` and
`--window <tokens>` override them, as the `kgraf worker` flags of the same
names do. The dry-run estimate is the rounds times the output ceiling,
priced at the output rate.

No route is ever refused for price, and this bench sets no spend cap of its
own; a spend limit belongs in the provider's own console, not here.

## Prices

Prices come from kGraf's vendored litellm catalog. `--prices <path>` overrides or adds
entries, in the JSON shape `kgraf orchestrate --prices` reads. When the catalog has no entry
for a model id, the bench prints a warning with the closest keys and never picks one. A
model with no price still runs. Its cost prints as "unpriced" ("local, unpriced" for a
local route) and is left out of the cost sums, never counted as $0.

## Judging an attempt made outside kGraf

`kgraf-bench-check` applies the suite's own check to a directory that another harness edited, so a baseline (aider, for
example) and kGraf's worker are judged by the same code. The caller copies a brief's fixture into a repository, commits
it, lets the other harness edit the files, commits the result, and passes the repository, the fixture commit and the
attempt commit:

```
kgraf-bench-check --brief impl-fizzbuzz --repo <dir> --base <fixture commit> --commit HEAD
```

It runs `check::verify` (protected paths against the fixture commit, build inputs refused, tests in the sandbox, mutants
for a `Tests` brief) and prints one JSON line. Exit status 0 is satisfied, 1 is not satisfied, 2 means the check could not
run (a harness fault, not a measurement). `--list` prints the suite (ids, kinds, versions, instruction text, protected paths,
fixture directories and the suite hash) as JSON, and `--self-test` runs the positive control.
