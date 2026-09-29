# Decision 0008: kGraf makes no token-savings claim

**Status:** Accepted
**Related:** decision 0009

## Context

### What the published evidence says

The commonly cited figure that multi-agent systems save tokens against a
single competent coding agent does not hold up. Anthropic's own published
figure for multi-agent research work is roughly 15x the token cost of a
single chat turn, and a single agent runs at roughly 4x chat, which puts the
multi-agent-to-single-agent multiple at about 3.75x, with a plausible range
of 2.9 to 4.9. That is a cost increase, not a savings.

Anthropic names coding explicitly as a poor fit for the multi-agent
research pattern, on the grounds that most coding tasks have fewer truly
parallelizable subtasks than research does. That means the overhead should
be larger for coding, not smaller, pushing the true multiple above the
modeled one.

The cleanest matched comparison available favors the simpler system: on the
same backbone and benchmark, a non-agentic pipeline resolved more issues at
roughly one third the cost of a multi-agent system.

No published study equalizes the token budget between single-agent and
multi-agent arms on a repository-level coding benchmark. The one
budget-matched study found excludes code execution, and favors single
agents. Every claim about multi-agent coding efficiency currently in
circulation is confounded with spend.

Two practitioner reversals are visible on public leaderboards: one team
merged two agents back into one after observing context loss between them;
another, at a high score on a public verified benchmark, explored dedicated
planning and testing agents and reverted to a single primary agent.

### The candidate defenses, and which hold

Four defenses for a multi-agent architecture's cost were considered. Graph
slices and narrow briefs genuinely attack the token multiple, by keeping
each worker's context small. Cheap-model routing attacks price only, and
only if the single-agent baseline runs a more expensive model than the
multi-agent worker mix; otherwise it can make the multi-agent system more
expensive per token. Cache-friendly prefixes cannot reduce token count at
all, because a cached token is still a counted input token.

Caching in fact structurally favors the single agent: a short-lived worker
pays roughly 3.3x more per turn for the same prefix than a long-lived solo
agent, because amortization is proportional to the number of turns riding
the prefix. Fan-out into many short workers is the worst possible shape for
prompt caching.

No defense touches the duplicated-work bucket at all, and step repetition
is measured at 13.2 percent of multi-agent failures in the interference
literature (decision 0009).

### The baseline behind the headline figure

Any stated cost multiple must name its baseline model, because the baseline
choice moves the answer by roughly a factor of two. Using modeled figures
rather than measurements:

- **Tokens:** about 2.58x a single agent on the same task.
- **Dollars against a solo top-tier agent** (orchestrator on a top-tier
  model, workers a 60/40 mix of top-tier and mid-tier models): about 3.3x.
- **Dollars against a solo mid-tier agent** (orchestrator on a mid-tier
  model, same worker mix): about 5.1x. With the orchestrator on the
  top-tier model instead, about 6.7x.

## Decision

kGraf does not claim to reduce token spend, and no document, README, or
interface copy may imply it does.

The stated expectation is that kGraf costs roughly 2.5 to 3x the tokens and
3 to 5x the dollars of one competent single coding agent, and any statement
of that figure must name the baseline model.

kGraf is justified on control, visibility, provider independence, and
verifier-selected parallel attempts. Not on price.

Any benchmark of this cost multiple must permit and publish a negative
result, and that criterion must not be relaxed.

## Consequences

**The honest sentence, for use in documentation:** kGraf should expect to
cost roughly 3 to 5x a single coding agent and must justify that on isolated
parallel attempts with verifier-based selection, provider independence, and
local control, not on price. The published record contains no case where
multi-agent coordination on a shared codebase paid for itself in tokens, and
cases where it measurably did not.

**Cost per successful task is the metric, not cost per token.** A 3x cost
multiple at half the success rate is a 6x cost per resolved issue, and
uncoordinated parallel coding agents have a measured 23.3 percent pair-pass
rate (decision 0009).

**Token frugality work still happens**, because reducing a 5x multiple to a
3x multiple is worth real money even without a savings claim. Priority order
follows measured effect: deferred tool loading (minus 85 percent of
tool-definition tokens with accuracy improving, and caching preserved),
context editing (minus 84 percent over 100 turns with task performance up
29 percent), graph slices (minus 23 percent, the only measured retrieval
figure), structured returns (minus 25 percent with accuracy up), and
cache-prefix discipline.

**Three things are explicitly not adopted**, because the evidence says they
backfire. Prompt compression inside a cached loop is net negative, measured
at minus 38 percent tokens and plus 6.8 percent billed cost, because
stochastic output means a new prefix every turn. Batch APIs are useless
inside an agent loop. Tool-result truncation has no published measured
saving and is folklore.

**Timer-driven compaction is refused.** Compaction invalidates the cache
prefix at the clear point, so it must be gated on measured net benefit and
clear at least 15 percent of the prefix, not fired on a timer.

**Any kGraf mechanism claiming a saving must be tested for the saving, not
for the config setter.**

**Reactivation trigger:** a benchmark run inside this project, or a
published budget-matched single-versus-multi study on a repository-level
coding benchmark. The latter does not currently exist.

## Alternatives considered

**Claim savings and hope.** Rejected. It would be a claim the evidence does
not support, and it would be a lie.

**Claim savings conditionally, with caveats.** Rejected. Conditional claims
get quoted without their conditions, and the evidence does not support even
the conditional version at present.

**Stay silent on cost.** Rejected. The product shows a live cost readout,
so users will form an impression regardless. Better to set it correctly.

**State the multiple honestly and justify on other grounds.** Chosen.
