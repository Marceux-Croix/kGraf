# Decision 0052: Scheduled runs are in scope, and the app may run in the background

**Status:** Accepted
**Related:** decisions 0014, 0017

## Decision

1. **kGraf supports scheduled runs.** A scheduled run is an orchestrator run started by the
   clock rather than by a manual play control. It comes in two kinds: recurring, on a
   standard calendar recurrence rule, and one-shot, at a stated instant.

2. **The desktop app may keep a background process alive while its window is closed**, so
   that schedules can fire. The process is visible: it has a system-tray presence, and the
   settings screen can turn background running off entirely, which suspends every schedule.

3. **A schedule is a property of a work node, rendered on the graph.** It is not a separate
   list of jobs. There is no board or job list anywhere in the product, and no second place to
   look for scheduling state outside the graph. The settings screen holds only the global
   on/off switch and default limits.

## Mechanism requirements

These are binding requirements for when the feature is built.

- **Due occurrences are claimed atomically** in local storage before they execute, so a crash
  between the claim and the run never fires the same occurrence twice.
- **Readiness is checked before capacity is claimed.** An occurrence checks its credentials,
  its MCP servers, and complete tool discovery before it takes write leases or a concurrency
  slot.
- **Failures are typed.** A missing credential, missing configuration, or a revoked permission
  disables the schedule and states why. A transient outage counts toward a
  consecutive-failure threshold rather than disabling the schedule immediately. A credential
  store being temporarily unreachable is never read as proof that the credential itself is
  missing.
- **An unknown timezone is refused when the schedule is created**, rather than silently
  falling back to the machine's local time.
- **Recurrence evaluation is bounded in time**, because a maliciously or accidentally crafted
  recurrence rule can be expensive to expand.
- **Every run is recorded** with its outcome, as history attached to the work node.
- **Spend is capped per run** and checked before the run starts.

## Consequences

The desktop app gains a tray presence and a background mode alongside its normal window.

Because scheduling makes starting a run free to the user, both the number of scheduled runs
and their total spend are reported, not just their individual outcomes, since a mechanism that
makes an action cheaper tends to increase how often that action is taken.

Scheduling depends on the orchestrator existing first; nothing here is built before that.
