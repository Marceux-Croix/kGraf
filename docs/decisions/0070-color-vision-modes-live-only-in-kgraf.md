# Decision 0070: Color vision modes live in kGraf's own theme code, chosen by measurement

**Status:** Accepted
**Related:** decision 0066; kGraf's style guide

## Context

kGraf's visual design uses five meaningful colors (progress, danger, needs input, selection,
neutral) to carry state across the application and the code graph. A user with a color vision
deficiency needs those five meanings to stay distinguishable from one another, which the
application's single default palette does not guarantee.

## Decision

kGraf adds four color vision modes beside the standard one: for the two most common forms of
red-green color blindness, for blue-yellow color blindness, and for total color blindness.
Each mode has its own table of the five meaningful colors, chosen specifically for what that
condition would actually let a user perceive, rather than for what a sighted person sees.

**Each mode's colors were chosen by simulation, not by guesswork.** Each candidate palette was
run through a published, peer-reviewed simulation of the relevant color vision deficiency, and
the distance between every pair of the five meanings was measured with a standard,
perceptually calibrated color-difference formula. A palette is accepted only once every pair
of meanings clears a minimum measured distance under simulation, for every background surface
the application draws them on.

**Total color blindness gets its own, lower bar.** With only brightness to work with, and a
minimum readable-text contrast also required at the same time, no five-color palette can
reach the same separation the other three modes reach; its own bar is set to what is
achievable under both constraints together, not lowered to match the easier modes.

**Color is never the only cue.** Every meaningful color is already paired with a word, an
icon, or a shape wherever it appears, and for total color blindness this becomes the primary
way a user tells the five meanings apart, with the five brightness levels ordered consistently
as a secondary cue.

**The setting applies live**, without restarting the application, everywhere color is drawn:
the standard interface and the code graph read the same chosen mode.

## Consequences

- Ordinary readable-text contrast requirements are checked for every mode, on every surface
  color is drawn against, including a solid card background over the base color and a menu
  background, not only the plain page.
- A single shared palette safe under every form of color blindness at once was considered and
  rejected: it would be more constrained than tuning one palette per condition, giving a worse
  result for a user who knows their own condition.

## Alternatives considered

- **Simulate color vision deficiency at paint time, for every draw call.** Rejected: it would
  touch every part of the renderer and add a cost to every single frame, for no better a
  result than five colors chosen once, in advance, to survive the same simulation.
- **One single "colorblind-safe" palette for every condition.** Rejected: a palette safe under
  every condition at once is necessarily more constrained, and less well-suited to any one
  condition, than a palette tuned specifically for it.
