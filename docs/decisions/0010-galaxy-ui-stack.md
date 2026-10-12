# Decision 0010: A server-computed, cached, anchored graph layout

**Status:** Accepted. The original renderer choice is superseded by a later
decision; the server-computed, cached, anchored layout and the density rules
stand.
**Superseded by:** 0054 (in part: the renderer choice)
**Amended by:** 0015 (the stability property), 0058 (the layout stays solver-free)
**Related:** decisions 0002, 0003, 0007

## Context

### Why server-side layout

A layout endpoint that returns nodes with baked coordinates, size, and
color means the rendering surface never runs a force simulation, and
instance matrices are built once rather than touched per frame. This is one
of the most important architectural properties a rendered graph of this
kind can have.

The alternative is visible in browser-simulated force layouts, which
degrade to a hairball well before ten thousand nodes: quadratic repulsion
and one render element per node scale badly, and reference implementations
of that approach are explicitly scoped to under 100 nodes and 200 edges.
kGraf needs to render on the order of ten thousand nodes.

### Why anchored rather than free

A plain force layout on tens of thousands of code nodes becomes a hairball.
Anchoring each node to a deterministic ring position derived from a hash of
its directory path separates clusters by construction rather than by
emergent physics, and makes layouts stable across reindexes.

The refinement is gentle: roughly 40 iterations of octree repulsion and edge
attraction, with an anchor spring at a quarter of node mass and a per-step
displacement cap.

### Why the density rules matter at scale

At scale, a washed-out center is caused by edges, not nodes, because
additive blending stacks tens of thousands of long lines through the
center. Edge intensity should dim on an inverse-square-root curve, holding
total glow roughly constant, while nodes and bloom stay at full strength to
a reference count and only ease to floors far beyond it.

A second effect is subtler: bloom is luminance-thresholded, and blue
carries almost no luminance weight, so a naive brightness boost blows out
white and yellow mid-degree nodes while leaving important blue hub nodes
flat. The fix boosts by channel dominance rather than by brightness.

Neither rule would be arrived at independently from first principles, and
both are the difference between a legible graph and a smear.

### Why the selection model highlights a one-hop neighborhood

Clicking a node computes its one-hop neighborhood, drops non-highlighted
nodes to a low color and scale, removes edges touching nothing selected, and
flies the camera to the centroid. This is exactly the agent-selection
requirement: select an agent, its live edges survive at full brightness,
everything else recedes.

### Layout caching must be real, not promised

A layout cache that is only a header comment, with every request
recomputing the layout from scratch, does not survive an interactive
multi-agent graph. Agent-driven views re-request constantly, and positions
must be stable across reindexes or agents will appear to teleport.

## Decision

The graph renders with instanced 3D geometry through a scene graph library.

**Layout is computed in the Rust core and cached in the database**, not
simulated in the render surface. The render surface receives baked
positions, sizes, and colors.

The layout is **anchored**: deterministic directory-ring seeding, depth on
the third axis, then gentle force refinement against an anchor spring. Not
a free force graph.

**Agents get their own visual channel**, distinct from the code nodes'
existing degree-driven color and size encodings.

Density rules for rendering at scale (edge dimming, luminance-aware bloom)
follow the pattern described above.

## Consequences

**Scene parameters**: instanced meshes with vertex colors and tone mapping
off, a point-sprite cutover above 75,000 objects, sphere tessellation
degrading by node count, a single line-segments object with additive
blending and no depth write, no edge bundling, and labels capped at 80
chosen by descending size.

**Budget for the point-sprite cutover sooner than a pure code graph would**,
because agents and code share one scene.

**Agents must not reuse the degree-driven encodings.** Color is stellar
class by degree and size is label class plus degree. Agents need distinct
geometry, and their live read and write links should render at the higher
opacity reserved for cross-cluster edges.

**Agent coordination edges live in the same edge table as code edges**
(decision 0007), typed like any other, so selection highlighting and
filtering work on them with zero new code.

**Bind edge opacity to the call edge's confidence property.** Call
resolution is explicitly probabilistic in the source graph, and showing a
guess at full strength misrepresents it.

**Attribution is required where third-party density, layout, color, and
scene modules are reused closely**, per their permissive licenses. Retain
the original copyright notice. Third-party grammars and the rendering
library used are permissively licensed, with nothing copyleft.

## Alternatives considered

**A browser-computed force layout with vector graphics.** Native hit
testing, crisp text, live theming, and no canvas teardown are real
advantages, and that reasoning is correct for its scale: it is well suited
to under about 100 text-heavy interactive nodes. The answer is different at
kGraf's target scale.

**A JavaScript force-layout library, or a general-purpose graph rendering
library.** Rejected: a server-computed layout makes the browser-side layout
engine irrelevant. The remaining job is instanced rendering, which the
chosen renderer does directly.

**Client-side layout on a worker thread.** Rejected: it does not fix
stability across reindexes, and it puts the expensive computation on the
weaker side of the boundary.

**Instanced 3D rendering with a server-computed cached layout.** Chosen.
