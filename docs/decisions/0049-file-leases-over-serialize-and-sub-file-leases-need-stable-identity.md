# Decision 0049: File leases over-serialize about half of their waits; sub-file leases need stable identity first

**Status:** Accepted. Amends the acceptance criterion in decision 0022 and extends the
granularity note in decision 0048.
**Related:** decisions 0010, 0015, 0022, 0048

This record exists so the next review of write-lease granularity does not have to re-derive
either measurement below, and so the earlier acceptance criterion is not read as a test it
cannot fail.

## Decision

Four conclusions, each following from a measurement or a defect found while investigating
lease granularity.

1. **The earlier serialization-rate acceptance criterion is replaced.** It asked whether the
   observed serialization rate lands near the project's baseline collision rate rather than
   near the maximum possible rate. Under whole-file leasing this cannot fail: a file-level
   lease fires exactly when two workers want the same file, which is the definition of the
   baseline collision rate, so the two are equal by construction. The criterion restates the
   mechanism's own definition and measures nothing. The acceptance criterion is replaced with
   the **needless-wait fraction**: of the worker pairs the registry actually serialized, the
   share whose declared write ranges did not truly overlap. The registry already records
   declared ranges, so this is a direct query against its history.

2. **Sub-file leasing is worth building, and the benefit is about 1.9x, not 4.8x.** On
   near-concurrent commit pairs (representative of what a live registry would actually see),
   47.9 percent of the pairs that share a file touch no overlapping region of it.
   Region-level exclusion would cut serialization from 0.229 to 0.119 in that population.
   Treat that as an upper bound. An earlier, larger figure (79.1 percent, a 4.8x reduction)
   was measured over every pair of commits in each sampled repository, most of them months
   apart; that population is not what a live lease actually sees, and it is withdrawn as the
   headline number.

3. **A code element's identity is hierarchical and signature-derived, not positional.** A
   child element's identity derives from its parent's identity, its own name, and a digest of
   its own signature (the source from the element's start through its body opening, not
   including the body). An edit inside a function's body therefore does not change that
   function's identity. A simple position-in-file ordinal survives only as a fallback for
   distinguishing identically named siblings that the signature-based scheme cannot tell
   apart, and such elements are marked as having unstable identity.

4. **A sub-file lease must refuse to be taken on an element with unstable identity**, and
   fall back to leasing the whole file instead. Such an element has no identity that survives
   an edit, so a lease on it would end up protecting whatever code a plain ordinal happens to
   point at next. Declaring that limitation and falling back to the coarser, safe grain is the
   correct response; silently guessing would be a silent substitution.

## Context

### The granularity measurement

Using a fixture of real commit history that records, per commit, the exact line ranges each
commit changed in each file, pairs of commits were compared for whether they shared a file and
whether their changed ranges within that file actually overlapped (within a small margin for
context).

| Population | Pairs | Share a file | Overlap in region | Over-serialized | File-level rate | Region-level rate |
|---|---:|---:|---:|---:|---:|---:|
| Near-concurrent (a handful of commits apart) | 2,700 | 618 | 322 | **47.9%** | 0.229 | 0.119 |
| Every pair in each repository | 74,545 | 8,333 | 1,741 | 79.1% | 0.112 | 0.023 |

**Why the all-pairs figure is larger.** Commits months apart tend to share files through
long-lived, broadly touched files such as documentation and manifests, and rarely touch the
same lines within them. Work done near in time tends to cluster on the same code, so more of
those file-sharing pairs genuinely overlap. A live lease only ever sees concurrent work, so
the near-concurrent population is the relevant one.

**Why both figures are upper bounds on the real benefit.** History that reached a shared
branch tends to have already had its real collisions rebased, squashed, or abandoned before
merging, so true overlap is under-represented in any such fixture. And the fixture's regions
are diff hunks, which are finer-grained than any unit a real lease would use: a lease would be
taken on whole functions or similar code elements, which are at least as large as the hunks
inside them, so two hunks that miss each other in this measurement could still fall inside the
same leased element in practice. That finer, element-level overlap could not be computed from
this fixture, since it does not carry the underlying source.

### The identity defect

An earlier code-element identifier combined a file path, an element kind, its qualified name,
and an ordinal counting identically named siblings in source order. Inserting an
earlier-appearing, identically named element renumbered every later sibling's ordinal. The
resulting failure was worse than an identifier simply going stale: the identifier silently
rebound to a different piece of code. This was reproduced directly: given two identically
signed methods that differ only in their bodies, inserting a third, identically named method
above them causes the first method's old identifier to now name the inserted method instead. A
lease, a cached position, or a subscription held against that identifier would silently point
at code nobody asked for, with nothing reporting the mismatch.

An existing stability test had not caught this, because its two example functions had distinct
names, so no ordinal was ever consulted; it proved stability only for the distinct-name case
and had been read as a claim about all identifiers.

A related defect in the signature computation itself, where a one-line function's signature
included its entire body, meant a body edit alone could move the signature. This was caught as
soon as the signature became the primary discriminator for identity.

## Alternatives considered

**For the identifier.**

- **Line range.** Moves on any edit above the element.
- **Content hash of the whole element.** Changes on exactly the edit the identifier must
  survive.
- **Positional ordinal.** This was the original defect: it can rebind silently.
- **Tracked identity across snapshots**, using similarity matching to carry identifiers
  through renames. Deferred; it needs a history of observed snapshots, not a single parse, and
  it is the only option that would survive a rename.
- **Hierarchical signature digest, with a declared-unstable fallback.** Chosen. It survives
  edits to a body and edits elsewhere in the file, distinguishes otherwise-identical
  same-named elements under different parents, and declares when it cannot separate two
  siblings.

**For the acceptance criterion.**

- **Keep the original serialization-rate wording.** Cannot fail under file-level leasing.
- **Require region-level leasing immediately.** The measured benefit is an upper bound near
  1.9x, and the registry had not yet run under real workers; building finer exclusion before
  the needless-wait fraction is observed in practice would repeat the same
  measure-before-building error found elsewhere in this project.
- **Report the needless-wait fraction from recorded declared ranges.** Chosen.

## Consequences

The acceptance line for this phase of work changes to the needless-wait fraction, not the
serialization rate against the baseline collision rate.

The earlier reactivation trigger, phrased as "serialization rate near the maximum," could
never fire for the same reason the criterion itself could not. It is restated as: sub-file
leasing is reactivated once the observed needless-wait fraction, measured from real use, lands
near the roughly 0.48 figure measured here. If it is near zero instead, file-level granularity
is fine in practice and the question closes.

A rename still breaks identity under this scheme, since a signature-derived identifier changes
when the name or signature changes. That is the honest limit of a single-snapshot identity
scheme, and it is exactly what a planned identifier-survival measurement across real commit
history is meant to quantify.

Every consumer of a code element's identifier (cached positions, subscriptions, and leases)
must check whether that identifier's identity is marked stable. A consumer that ignores this
flag reintroduces the rebinding defect for exactly the elements where it can occur.
