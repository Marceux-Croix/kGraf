# Decision 0017: Bounded autonomy, a user-facing concurrency cap, and a settings surface

**Status:** Accepted. Amends the scope of the no-second-surface rule from
decision 0003 to state the boundary explicitly: settings holds
configuration only.
**Superseded by:** 0067 (in part: the starting concurrency numbers and fixed maximums), 0080 (in part: the spend-cap parts)
**Related:** decisions 0003, 0008, 0014, 0016

## Context

Removing the item boundary (decision 0016) let the eligible frontier become
arbitrarily wide, so concurrency stopped being bounded by structure. Making
decomposition recursive (decision 0014) means agents can beget agents.
Together those are an explosion shape, and nothing in the design otherwise
stops it.

The reasons for a cap are both cognitive and economic: too many concurrent
items is overwhelming to watch and burns tokens; too few accomplishes
nothing useful. Both halves matter, and the tradeoff between them is not
one a default can settle permanently, which is why it is a user control
rather than a constant.

### Why pre-flight enforcement, specifically

A limit enforced only after the cost is incurred is not a limit. Some
surveyed reference systems set a very high default token cap per agent,
made enforcement advisory, and used a reaper that killed the worker only
after the spend had already happened, with a test that asserted only that
the configured value persisted. That failure mode is exactly what a
pre-flight check must avoid: the check itself, not the setting that
describes it, must be the thing under test.

### Why a settings surface does not violate the no-second-surface rule

Configuration is not work state, so a settings surface duplicates nothing
the graph already shows. The risk is drift: a settings panel that grows a
status readout, a run history, or an activity list has become the second
place to look that the no-second-surface rule exists to prevent.

## Decision

**Play means autonomous within predefined limits, not unbounded.** The
user does not approve each partition; the orchestrator schedules and the
user watches. But kGraf cannot spawn agents without a ceiling.

**A user-facing concurrency control sets how many work items may be in
flight at once.** It lives on a slider with a stated default, because the
right value is a judgment the user makes and changes.

**A hard ceiling on total live agents is enforced separately**, because
recursive decomposition means one item's worker can itself cause more
agents.

**Both limits are enforced pre-flight.** A spawn that would exceed a limit
is refused, never started and then killed.

**A settings surface exists**, scoped to configuration only. It never
displays work state.

## Consequences

**Two limits, not one.** Concurrent work items is the user-facing control.
Total live agents is a separate ceiling that recursive spawning cannot
breach. Naming them distinctly matters, because a user setting items to
four and seeing eleven agents would reasonably consider the control broken.

**The default starts in a small band, roughly three to five.** Multiple
surveyed agent systems default their concurrency to a small number in this
range, and documentation for at least one notes that a handful of focused
workers often outperforms more scattered ones, with diminishing returns
beyond a point. This is a starting default to be tuned by measurement, not
a settled number.

**Enforcement is tested, not the setting.** The acceptance test attempts to
exceed each limit and asserts refusal. A test that sets the slider and
reads it back does not count.

**A refused spawn is a visible state, not a silent drop.** Under decision
0014 the interface distinguishes blocked from colliding; a node waiting on
the concurrency cap is a third reason, and it must be surfaced rather than
the node simply sitting still.

**Settings is configuration only.** Model and provider preferences,
concurrency limits, credentials, and project paths belong there. Anything
describing what is happening right now belongs on the graph or in the
orchestrator's answer.

## Open

A spend cap is a separate control from a concurrency cap and is not
decided here. Concurrency bounds how much runs at once; it does not bound
what a single long-running job costs. Whether kGraf carries a budget limit,
and whether it warns or halts on approach, is unresolved. Every call is
already attributed under the accounting design, so the data to enforce one
exists; only the policy is missing.

## Alternatives considered

**Approve each partition before agents spawn.** Rejected. Decision 0015
made partitioning continuous, so approval would be constant, which is the
altitude loss the product exists to prevent.

**Unbounded autonomy on play.** Rejected on both the cognitive and economic
grounds above. It is also the failure mode in the 16-agent build case cited
in decision 0009, where many agents produced nothing useful until the work
was forcibly made disjoint.

**A fixed internal constant instead of a control.** Rejected. The right
value depends on the project, the models in use, and the user's tolerance,
and none of those are knowable in advance.

**A cap on concurrent items only.** Rejected as insufficient, because
recursive decomposition means item count and agent count are not the same
number.

## Amendment (2026-10-01): run limits are global user settings

Five run limits are settings the user changes once, for every run, with a slider each in
Settings > General (no new panel; configuration only, so 0017's boundary holds). They are
keys in the user-level `settings.toml`, the file a worker cannot write, and the command
line reads the same keys.

| Setting key | Slider range (step) | Built-in default |
| --- | --- | --- |
| `rounds_per_worker` | 4 to 200 (1) | 30 orchestrated, 12 for `kgraf worker` |
| `max_checkpoints` | 1 to 50 (1) | 10 |
| `worker_max_tokens` | 1,024 to 131,072 (1,024) | 16,384 (was 4,096) |
| `planner_max_tokens` | 4,096 to 131,072 (1,024) | 32,768 (was 8,192) |
| `context_window` | 8,192 to 1,000,000 (1,024) | 128,000 |

- **Precedence:** flag, then user setting, then built-in default. A key in `.kgraf/run.json`
  counts as a flag for its project. A slider that was never moved writes no key, so a later
  change of a default reaches a user who never chose a value. A settings value outside the
  bounds is an error naming the key, in the app and on the command line.
- **The checkpoint ceiling (decision 0096).** `rounds_per_worker` is the size of one block
  of rounds; `max_checkpoints` is the most blocks a worker may be granted while it makes
  measured progress. 1 means the old fixed round limit. It reaches every worker the app or
  the command line assembles, orchestrated or single, as `LoopSettings::max_checkpoints`,
  and the command flag is `--max-checkpoints` on `orchestrate` and `worker`.
- **One rounds setting, two defaults.** A single `kgraf worker` run is one probe and keeps
  its smaller default of 12 until a value is set; an orchestrated worker is one share of a
  plan and defaults to 30.
- **Clamped to the model, with a notice.** An output cap above a model's supported output
  limit (the bundled model catalog's `max_output_tokens`) is lowered for that run and
  reported at the run's start. The worker cap is the lowest limit among the worker routes,
  which rotate; a model the catalog does not list is not clamped. Settings shows the same
  sentence under the slider before a run starts.
- **The window slider and the ceiling.** A single worker still uses at most 100,000 tokens
  of the window (0057), so a window above that does not raise a worker's use.
- **When a change applies.** Runs started after it. A project's host is reopened when no
  run is active, and a change made during a run waits for it to end.

## Amendment (2026-10-02): one on or off setting beside the limits

Settings > General gains "Warn about rule conflicts", a checkbox (configuration only, so
0017's boundary holds), off by default. It is the key `warn_rule_conflicts` in the same
user-level `settings.toml`, read by the same reader, shared with the project hosts in the
same `SharedLimits` value, and resolved flag, then setting, then default (off). It limits
nothing; decision 0094's 2026-10-02 amendment says what it does. A change reaches runs
started after it, as the sliders do. A key in `.kgraf/run.json` counts as a flag for its
project. The key is written only while on, so a user who never turned it on has no key. The
control is a checkbox like the page's other toggles, until a switch widget exists
(`STYLE_GUIDE.md` CMP-SET-4).

## Amendment (2026-10-06): a sixth run limit, the answer wait

`answer_wait_secs` (30 to 1,800 s) joins the run limits by the same path: a slider in
Settings > Runs ("Answer wait (seconds)", steps of 30), a key in the user settings file, a
`run.json` key, `--answer-wait` on `orchestrate` (the setting for `schedule run-now`), and a
`limits` line with its source. Unlike the other five it has no fixed default: unset, each answer
call derives its wait from the answering model's recent latencies (decision 0067, amendment of
2026-10-06). The slider states that default in words, not as the number it rests at, and writes
no key until moved.
