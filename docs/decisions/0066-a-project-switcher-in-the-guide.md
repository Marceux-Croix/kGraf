# Decision 0066: Several projects open in one window, switched from the navigation rail

**Status:** Accepted
**Related:** decisions 0017, 0032, 0034, 0054, 0063

## Context

kGraf's standing design keeps the code graph as the only work surface, and rejects any
duplicate surface that shows the same information a second way. A user working on more than
one project at a time still needs a way to see which one needs attention, without that view
turning into a second work surface of its own.

## Decision

**One window hosts several open projects at once.** Each keeps its own scheduler, its own
graph and its own runs. Only one project is shown at a time. Switching changes only what is
displayed: the other projects' runs keep going in the background, and the window checks each
open project every frame, so a background run still advances.

**The switcher is a list in the navigation rail**: one row per project, with a name, one line
of state, and a status dot. A collapsed view of the same list carries an aggregate summary,
naming which projects need attention and why. Closing a project is refused while it is
actively working; the close is queued and completes once the project is idle.

**The dot is derived from live state, never stored.** It is yellow when the project needs
input (an approval, a failure); cyan when new results have landed since the project was last
viewed; gray and blinking while it is actively working; and off when idle. Priority is fixed:
needing input outranks new results, which outrank active work. A failure is always folded
into "needs input," since a failure needs the same kind of attention an approval does.

**The list of open projects is configuration, not work state.** It records which folders are
open, their order, and which one is shown; it records no counts of work or runs. Two windows
sharing one configuration file each keep their own edits without clobbering the other's.

## Consequences

- The switcher shows navigation only: it has no button that approves, retries, or reorders
  work. Clicking a row changes the view; it does not act on the underlying project. This
  keeps the code graph the project's only work surface.
- Every open project costs its own background threads, whether shown or not.
- A folder that no longer exists, or cannot be opened, stays in the list with its problem
  shown as its state line, rather than disappearing silently.

## Alternatives considered

- **One window per project.** Rejected: someone working several projects wants them in one
  place, with a single view of which one needs attention.
- **A tab strip.** Rejected: tabs suggest documents, not projects with their own live state,
  and leave no room for a status line per entry.
- **One shared scheduler across all open projects.** Not taken: a project's own configuration
  (its routes, its cost limits) would otherwise mix with another project's, which this design
  keeps strictly separate. Left open for the future.
