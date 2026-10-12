//! Layout positions.
//!
//! The reference implementation recomputes layout on every request, so agents
//! and code elements teleport between reindexes. kGraf computes a position
//! **as a pure function of the node's identity, its own call depth, and its
//! cluster's population bucket**, and caches it in the database (decision
//! 0007). There is no force solver.
//!
//! - **Stability across a reindex.** The same graph gives the same positions,
//!   in any insertion order, so nothing moves that did not change.
//! - **Local perturbation, not global stasis** (decision 0015). An insert moves
//!   another node only in the two cases listed under "When a node may move"
//!   below, never a node of another cluster that it does not call into.
//! - **A position for a node that did not exist at index time.** Decomposition
//!   creates work nodes mid-flight; a pure function gives each one a place
//!   without a global solve.
//!
//! # What is ported, and from where
//!
//! Ported from codebase-memory-mcp (MIT, Copyright (c) 2025 DeusData,
//! `src/ui/layout3d.c` and `graph-ui/src/lib/density.ts` at commit
//! `64c23fab6c3eba3de226355c0ca1ff750116edab`, license verified at the source on
//! 2026-09-26; notice in `third_party/codebase-memory-mcp/`):
//!
//! - **Ring seeding by directory.** The cluster key is the first three directory
//!   components, hashed with FNV-1a. The angle comes from the low 16 bits and the
//!   radius from the next 8 bits over the 500 to 750 band, so a directory lands
//!   at one angle and clusters separate by construction, not by physics.
//! - **Z from call depth**, 50 units per level, entry points on top
//!   ([`call_depths`], [`z_for`]).
//! - The dot size table ([`stellar`]) and the density rules
//!   ([`density`]), as data functions the renderer consumes.
//!
//! Deviations from the source, each deliberate and measured (synthetic graphs,
//! 2 calls per node, 80% inside the cluster; the measurements were recorded
//! when the layout was built):
//!
//! - **No Barnes-Hut refinement, global or per cluster.** Global: inserting one
//!   unconnected node moved all 5,000 existing nodes, 2,415 of them by more
//!   than a unit at 1,000 or more units from the newcomer (up to 30), because
//!   the median node ends near the 320-unit ceiling of 40 capped steps and the
//!   anchor spring does not hold. Refining only the new node makes its position
//!   depend on insertion order and differ by 86 units from a rebuild (0007).
//!   Refining each cluster alone keeps other clusters still, but one insert into
//!   a 2,000-node cluster moved all 2,000 of its nodes (up to 135 units with
//!   upstream's constants, 20 with gentler ones) and costs 190 to 270 ms at
//!   10,000 nodes. The population-scaled disc below matches upstream's spread
//!   at 5 ms with no motion on most inserts.
//! - **Node spread is a blake3-seeded disc whose radius grows with the
//!   cluster's population bucket** ([`cluster_radius`]), rather than
//!   upstream's 15-bit LCG square of side 40 followed by refinement. A 15-bit
//!   generator gives 32,768 values per axis, so two nodes in a large directory
//!   can land on the same point.
//! - **A small Z jitter** ([`Z_JITTER`]) inside each depth layer, so a layer is a
//!   slab rather than a flat sheet. Upstream gets that thickness from the
//!   refinement.
//! - **The cluster key is the first three directory components**, not the first
//!   three path components. Upstream's key includes the file name for any path
//!   of three or fewer components, which puts every shallow file in its own
//!   cluster, and its edge renderer uses a different (two-component) key. One
//!   key serves both here ([`cluster_key`]).
//! - **Call depth roots are the strongly connected components with no caller
//!   outside themselves**, not upstream's labels (Route, File, Module, Package)
//!   with a zero-in-degree fallback. See [`call_depths`] for why.
//!
//! # When a node may move
//!
//! A node's X and Y are a function of its directory, its id and its cluster's
//! population bucket. Its Z is [`z_for`]`(id, depth)`. So an insert or removal
//! elsewhere moves a node in exactly two cases:
//!
//! 1. **Its cluster crosses a power of two above 64 nodes.** Every node of that
//!    cluster scales by `sqrt(2)` about the cluster center (inward on removal).
//!    X and Y change; Z does not.
//! 2. **Its own call depth changes.** Z changes by a whole number of layers; X
//!    and Y do not. Call depth is the shortest distance over `calls` edges from a
//!    root component, so inserting or removing a node `u` can change the depth
//!    of `v` only when `v` is reachable from `u` along `calls` edges (in the
//!    graph after insertion, or before removal): any new or broken shortest path
//!    to `v`, any root that gains or loses a caller, and any component merge all
//!    lie downstream of `u`.
//!
//! In every other case the cached position is kept bit for bit. The cache is
//! written by [`cache::refresh_layout`], which the indexer runs after call
//! resolution; the store carries a surviving node's cached position across a
//! file rewrite.

pub mod cache;
pub mod density;
pub mod stellar;

use crate::chunk::NodeKind;
use crate::store::EdgeKind;

/// A position in the galaxy field.
///
/// Three dimensions because the interface is a point field, and a flat layout
/// wastes the axis that separates entry points from their callees.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Inner radius of the directory ring (upstream `500.0f`).
pub const RING_INNER: f32 = 500.0;
/// Width of the ring band (upstream `250.0f`), so centers fall in 500 to 750.
pub const RING_WIDTH: f32 = 250.0;
/// Outer edge of the field: the ring's outer radius. Every cluster center is
/// inside it and every node within [`CLUSTER_RADIUS`] of it.
pub const FIELD_RADIUS: f32 = RING_INNER + RING_WIDTH;
/// Smallest radius of a directory's disc around its ring center, in the XY
/// plane. Clusters of up to 64 nodes use it; larger ones grow
/// ([`cluster_radius`]).
pub const CLUSTER_RADIUS: f32 = 40.0;
/// Disc radius per square root of cluster population (upstream has no such
/// constant; see [`cluster_radius`] for the measurement it comes from).
pub const CLUSTER_SPREAD: f32 = 5.0;
/// Vertical spacing between call-depth layers (upstream `Z_DEPTH_SPACING`).
pub const Z_DEPTH_SPACING: f32 = 50.0;
/// Half-thickness of a depth layer. Less than half the layer spacing, so two
/// adjacent layers never interleave.
pub const Z_JITTER: f32 = 15.0;
// The lowest point of layer d stays above the highest point of layer d + 1.
const _: () = assert!(2.0 * Z_JITTER < Z_DEPTH_SPACING);
/// Path components that make up a cluster key (upstream: 3).
pub const CLUSTER_KEY_COMPONENTS: usize = 3;

/// The seed position of a new node: call depth 0, smallest cluster disc.
///
/// This is what the store writes when a node first appears, before call edges
/// are resolved; [`cache::refresh_layout`] then moves it to [`position`] with
/// its real depth and cluster population. Pure: same inputs, same output, on
/// every machine and process.
pub fn position_for(directory: &str, node_id: &str) -> Position {
    position(directory, node_id, 0, 1)
}

/// The position of a node at a call depth, in a cluster of `population` nodes.
///
/// X and Y depend only on `directory`, `node_id` and the population's
/// power-of-two bucket ([`cluster_radius`]); Z only on `node_id` and `depth`.
pub fn position(directory: &str, node_id: &str, depth: u32, population: usize) -> Position {
    let (cx, cy) = cluster_center(directory);
    let hash = hash_bytes("node", node_id);
    let (dx, dy) = point_in_disc(&hash, cluster_radius(population));
    Position {
        x: cx + dx,
        y: cy + dy,
        z: z_from_hash(&hash, depth),
    }
}

/// The disc radius of a cluster holding `population` nodes:
/// `max(40, 5 * sqrt(next_power_of_two(population)))`.
///
/// Why it grows. A fixed disc draws a 2,000-node directory as a solid blob:
/// on a synthetic 10,000-node graph (clusters of 10 to 2,000 nodes), 56% of
/// nodes overlap another node's drawn sphere at radius 40. Growing the disc
/// with the square root of the population holds density constant. At a spread
/// of 5 the largest cluster's median distance to its centroid is 162 units
/// against upstream's 153 after refinement, and overlap falls to 8.4% (upstream
/// 3.0%) with no solver.
///
/// Why power-of-two buckets. A radius that followed the exact population
/// would move every node of a cluster on every insert into it. Bucketed, an
/// insert moves other nodes only when it carries its cluster across a power of
/// two above 64, and then it scales that one cluster by `sqrt(2)` about its
/// center: 5 times while a directory grows from 64 to 2,048 nodes. The
/// renderer should animate that rescale (decision 0015).
pub fn cluster_radius(population: usize) -> f32 {
    let bucket = population
        .max(1)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    (CLUSTER_SPREAD * (bucket as f32).sqrt()).max(CLUSTER_RADIUS)
}

/// The Z coordinate of a node at a call depth: `-depth * 50` plus the node's
/// fixed jitter. Entry points (depth 0) are on top.
pub fn z_for(node_id: &str, depth: u32) -> f32 {
    z_from_hash(&hash_bytes("node", node_id), depth)
}

/// The center of a directory's cluster on the ring.
///
/// Port of upstream's seeding: FNV-1a over the cluster key, angle from the low
/// 16 bits, radius from the next 8 bits over the 500 to 750 band.
pub fn cluster_center(directory: &str) -> (f32, f32) {
    let h = fnv1a(cluster_key(directory));
    let angle = ((h & 0xFFFF) as f32 / 65535.0) * std::f32::consts::TAU;
    let radius = RING_INNER + (((h >> 16) & 0xFF) as f32 / 255.0) * RING_WIDTH;
    (radius * angle.cos(), radius * angle.sin())
}

/// The cluster a directory belongs to: its first three components.
///
/// `crates/kgraf-graph/src/layout` and `crates/kgraf-graph/src` share
/// a cluster, so a deep tree does not scatter across the ring.
pub fn cluster_key(directory: &str) -> &str {
    match directory.match_indices('/').nth(CLUSTER_KEY_COMPONENTS - 1) {
        Some((i, _)) => &directory[..i],
        None => directory,
    }
}

/// The directory part of a repository-relative path, or `""` for a root file.
pub fn directory_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Call depth of every node: the shortest distance along `calls` edges from a
/// root, where a root is any node in a strongly connected component that no
/// node outside it calls.
///
/// `calls` holds `(caller, callee)` index pairs into `0..node_count`. The
/// result is indexed the same way. Pairs out of range are an error, never
/// skipped (fail loud).
///
/// Why components rather than upstream's rules. Upstream seeds depth 0 from
/// Route, File, Module and Package labels and walks every edge type. Because
/// upstream's File node `DEFINES` every definition, every definition sits at
/// depth 1 and the call walk never deepens anything: its "call depth" is in
/// practice two layers. Its fallback, zero in-degree, leaves a recursive entry
/// point and everything below it unreachable (depth 0). Taking the component
/// condensation makes every node reachable from some root, so no fallback is
/// needed, and self-recursion and mutual recursion do not flatten a subtree.
///
/// Linear time: Tarjan's algorithm (iterative, so a long call chain cannot
/// overflow the stack) plus one breadth-first pass over a compressed adjacency.
/// Upstream's walk scans every edge per dequeued node, which is quadratic.
pub fn call_depths(node_count: usize, calls: &[(u32, u32)]) -> Result<Vec<u32>, LayoutError> {
    let adjacency = Csr::build(node_count, calls)?;
    let component = strongly_connected(&adjacency);

    // A component is a root when no edge enters it from another component.
    let component_count = component.iter().map(|&c| c as usize + 1).max().unwrap_or(0);
    let mut entered = vec![false; component_count];
    for (src, dst) in calls {
        let (cs, cd) = (component[*src as usize], component[*dst as usize]);
        if cs != cd {
            entered[cd as usize] = true;
        }
    }

    let mut depth = vec![u32::MAX; node_count];
    let mut queue = std::collections::VecDeque::with_capacity(node_count);
    for node in 0..node_count {
        if !entered[component[node] as usize] {
            depth[node] = 0;
            queue.push_back(node as u32);
        }
    }
    while let Some(node) = queue.pop_front() {
        let next = depth[node as usize] + 1;
        for &callee in adjacency.targets(node) {
            if depth[callee as usize] == u32::MAX {
                depth[callee as usize] = next;
                queue.push_back(callee);
            }
        }
    }
    // Every component is reachable from a source component of the
    // condensation, so nothing is left unvisited. Checked anyway, loudly.
    if let Some(stranded) = depth.iter().position(|&d| d == u32::MAX) {
        return Err(LayoutError::Unreachable { node: stranded });
    }
    Ok(depth)
}

/// One node handed to [`layout_graph`].
#[derive(Clone, Copy, Debug)]
pub struct LayoutNode<'a> {
    pub id: &'a str,
    pub directory: &'a str,
    pub kind: NodeKind,
}

/// One edge handed to [`layout_graph`], by index into the node slice.
#[derive(Clone, Copy, Debug)]
pub struct LayoutEdge {
    pub src: u32,
    pub dst: u32,
    pub kind: EdgeKind,
}

/// What the renderer needs to draw one node.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NodeVisual {
    pub position: Position,
    /// Call depth, the input to Z.
    pub depth: u32,
    /// Degree over every edge kind, the input to size (upstream counts every
    /// mapped edge).
    pub degree: u32,
    /// Scene units, from [`stellar::node_size`].
    pub size: f32,
}

/// Lay out a whole graph: positions from identity and call depth, color and
/// size from degree and kind.
///
/// Positions come from [`position`] with each cluster's population counted
/// over `nodes`. The store caches the same function's output once
/// [`cache::refresh_layout`] has run over the same graph; between an insert
/// and that refresh, a new node sits at its [`position_for`] seed. Color and size are
/// derived at read time and never cached (decision 0007: facts are stored,
/// display is derived).
pub fn layout_graph(
    nodes: &[LayoutNode<'_>],
    edges: &[LayoutEdge],
) -> Result<Vec<NodeVisual>, LayoutError> {
    let n = nodes.len();
    let mut degree = vec![0u32; n];
    let mut calls = Vec::new();
    for edge in edges {
        let (s, d) = (edge.src as usize, edge.dst as usize);
        if s >= n || d >= n {
            return Err(LayoutError::EdgeOutOfRange {
                src: edge.src,
                dst: edge.dst,
                node_count: n,
            });
        }
        degree[s] += 1;
        degree[d] += 1;
        if edge.kind == EdgeKind::Calls {
            calls.push((edge.src, edge.dst));
        }
    }
    let depth = call_depths(n, &calls)?;
    let mut population: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for node in nodes {
        *population.entry(cluster_key(node.directory)).or_default() += 1;
    }
    Ok(nodes
        .iter()
        .enumerate()
        .map(|(i, node)| NodeVisual {
            position: position(
                node.directory,
                node.id,
                depth[i],
                population[cluster_key(node.directory)],
            ),
            depth: depth[i],
            degree: degree[i],
            size: stellar::node_size(node.kind, degree[i]),
        })
        .collect())
}

/// Why a layout could not be computed.
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    /// An edge names a node index that does not exist. Upstream skips such
    /// edges silently; here that would hide a caller bug (fail loud).
    #[error("edge {src} -> {dst} is outside the {node_count} nodes given")]
    EdgeOutOfRange {
        src: u32,
        dst: u32,
        node_count: usize,
    },
    /// A node was not reached by the depth walk. Unreachable by construction;
    /// raised rather than defaulted if the construction is ever wrong.
    #[error("node {node} was not reached from any call-graph root")]
    Unreachable { node: usize },
    /// A `calls` edge names a node with no cached layout row: the cache is out
    /// of step with the graph.
    #[error("node `{node}` has an edge but no cached layout position")]
    StaleCache { node: String },
}

fn z_from_hash(hash: &[u8; 32], depth: u32) -> f32 {
    let jitter = (unit(hash, 12) as f32 * 2.0 - 1.0) * Z_JITTER;
    -(depth as f32) * Z_DEPTH_SPACING + jitter
}

/// FNV-1a, 32-bit, as upstream's `fnv1a`. Kept for the cluster hash so a
/// directory's ring angle matches the reference for the same key.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for byte in s.bytes() {
        h ^= byte as u32;
        h = h.wrapping_mul(16_777_619);
    }
    h
}

fn hash_bytes(domain: &str, value: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"\x00");
    hasher.update(value.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Four hash bytes as a number in `[0, 1]`.
fn unit(hash: &[u8; 32], offset: usize) -> f64 {
    let bytes = [
        hash[offset],
        hash[offset + 1],
        hash[offset + 2],
        hash[offset + 3],
    ];
    u32::from_le_bytes(bytes) as f64 / u32::MAX as f64
}

/// Map hash bytes to a point uniformly distributed over a disc of `radius`.
///
/// The square root on the radius keeps the density even by area; a uniform
/// radius would pile every node at the cluster center.
fn point_in_disc(hash: &[u8; 32], radius: f32) -> (f32, f32) {
    let theta = unit(hash, 0) * std::f64::consts::TAU;
    let r = radius as f64 * unit(hash, 4).sqrt();
    ((r * theta.cos()) as f32, (r * theta.sin()) as f32)
}

/// Compressed sparse row adjacency over `calls`.
struct Csr {
    offsets: Vec<u32>,
    targets: Vec<u32>,
}

impl Csr {
    fn build(node_count: usize, edges: &[(u32, u32)]) -> Result<Csr, LayoutError> {
        let mut offsets = vec![0u32; node_count + 1];
        for &(src, dst) in edges {
            if src as usize >= node_count || dst as usize >= node_count {
                return Err(LayoutError::EdgeOutOfRange {
                    src,
                    dst,
                    node_count,
                });
            }
            offsets[src as usize + 1] += 1;
        }
        for i in 0..node_count {
            offsets[i + 1] += offsets[i];
        }
        let mut fill = offsets.clone();
        let mut targets = vec![0u32; edges.len()];
        for &(src, dst) in edges {
            let slot = &mut fill[src as usize];
            targets[*slot as usize] = dst;
            *slot += 1;
        }
        Ok(Csr { offsets, targets })
    }

    fn node_count(&self) -> usize {
        self.offsets.len() - 1
    }

    fn targets(&self, node: u32) -> &[u32] {
        let (a, b) = (
            self.offsets[node as usize] as usize,
            self.offsets[node as usize + 1] as usize,
        );
        &self.targets[a..b]
    }
}

/// Tarjan's strongly connected components, iterative. Returns a component id
/// per node. Local mutation throughout: this is the hot path of a layout, and
/// the explicit stack is what keeps a 100,000-deep call chain off the thread
/// stack.
fn strongly_connected(g: &Csr) -> Vec<u32> {
    const UNSEEN: u32 = u32::MAX;
    let n = g.node_count();
    let mut index = vec![UNSEEN; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut component = vec![UNSEEN; n];
    let mut stack: Vec<u32> = Vec::new();
    // (node, next edge position to examine)
    let mut call: Vec<(u32, usize)> = Vec::new();
    let mut next_index = 0u32;
    let mut next_component = 0u32;

    for start in 0..n as u32 {
        if index[start as usize] != UNSEEN {
            continue;
        }
        call.push((start, 0));
        while let Some(&mut (node, ref mut pos)) = call.last_mut() {
            let v = node as usize;
            if *pos == 0 && index[v] == UNSEEN {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(node);
                on_stack[v] = true;
            }
            let targets = g.targets(node);
            if let Some(&w) = targets.get(*pos) {
                *pos += 1;
                let wi = w as usize;
                if index[wi] == UNSEEN {
                    call.push((w, 0));
                } else if on_stack[wi] {
                    low[v] = low[v].min(index[wi]);
                }
                continue;
            }
            // All edges of `node` examined.
            call.pop();
            if low[v] == index[v] {
                while let Some(w) = stack.pop() {
                    on_stack[w as usize] = false;
                    component[w as usize] = next_component;
                    if w == node {
                        break;
                    }
                }
                next_component += 1;
            }
            if let Some(&(parent, _)) = call.last() {
                let p = parent as usize;
                low[p] = low[p].min(low[v]);
            }
        }
    }
    component
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distance(p: Position, q: Position) -> f32 {
        ((p.x - q.x).powi(2) + (p.y - q.y).powi(2) + (p.z - q.z).powi(2)).sqrt()
    }

    #[test]
    fn a_position_is_a_pure_function_of_identity() {
        let a = position_for("src/graph", "abc123");
        let b = position_for("src/graph", "abc123");
        assert_eq!(a, b);
    }

    #[test]
    fn inserting_a_node_moves_nothing_else() {
        // Local perturbation is the property under test, not global stasis: the
        // positions of existing nodes are computed without reference to the new
        // one, so they cannot move.
        let existing: Vec<_> = (0..50)
            .map(|i| position_for("src/graph", &format!("node{i}")))
            .collect();
        let _newcomer = position_for("src/graph", "node-inserted-later");
        let after: Vec<_> = (0..50)
            .map(|i| position_for("src/graph", &format!("node{i}")))
            .collect();
        assert_eq!(existing, after);
    }

    #[test]
    fn nodes_in_one_directory_cluster_together() {
        let a = position_for("src/graph", "one");
        let b = position_for("src/graph", "two");
        let far = position_for("ui/render", "three");
        // The XY disc plus the Z slab bound the distance between two members.
        let bound = 2.0 * (CLUSTER_RADIUS.powi(2) + Z_JITTER.powi(2)).sqrt();
        assert!(
            distance(a, b) <= bound,
            "same-directory nodes are {} apart",
            distance(a, b)
        );
        assert!(
            distance(a, far) > bound,
            "different-directory nodes are only {} apart",
            distance(a, far)
        );
    }

    #[test]
    fn positions_stay_inside_the_field() {
        for i in 0..500 {
            let p = position_for(&format!("dir{}", i % 20), &format!("n{i}"));
            let r = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
            assert!(
                r <= FIELD_RADIUS + CLUSTER_RADIUS + Z_JITTER + 1.0,
                "node {i} escaped at {r}"
            );
        }
    }

    #[test]
    fn directory_of_handles_root_files() {
        assert_eq!(directory_of("README.md"), "");
        assert_eq!(directory_of("src/lib.rs"), "src");
        assert_eq!(directory_of("a/b/c.rs"), "a/b");
    }

    #[test]
    fn distinct_ids_get_distinct_positions() {
        // A hash collision here would draw two elements on top of each other,
        // which reads as one element to the user. Two elements coincide only
        // when all three coordinates do. (X alone is not enough: a 40-unit disc
        // near radius 750 spans about 1.3 million f32 values, so 2,000 nodes
        // share an X by the birthday bound without sharing a point.)
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000 {
            let p = position_for("src", &format!("n{i}"));
            assert!(
                seen.insert((p.x.to_bits(), p.y.to_bits(), p.z.to_bits())),
                "duplicate position at {i}"
            );
        }
    }

    #[test]
    fn cluster_centers_lie_in_the_ring_band() {
        for i in 0..2000 {
            let (x, y) = cluster_center(&format!("pkg{i}/src"));
            let r = (x * x + y * y).sqrt();
            assert!(
                (RING_INNER - 0.01..=FIELD_RADIUS + 0.01).contains(&r),
                "cluster {i} center at radius {r}"
            );
        }
    }

    #[test]
    fn nodes_stay_within_their_cluster_disc() {
        for i in 0..2000 {
            let dir = format!("d{}", i % 7);
            let (cx, cy) = cluster_center(&dir);
            let p = position_for(&dir, &format!("n{i}"));
            let r = ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt();
            assert!(r <= CLUSTER_RADIUS + 0.01, "node {i} at {r} from center");
        }
    }

    #[test]
    fn the_cluster_hash_matches_the_reference_fnv1a() {
        // Published FNV-1a 32-bit test vectors.
        assert_eq!(fnv1a(""), 0x811c_9dc5);
        assert_eq!(fnv1a("a"), 0xe40c_292c);
        assert_eq!(fnv1a("foobar"), 0xbf9c_f968);
    }

    #[test]
    fn cluster_key_takes_the_first_three_directory_components() {
        assert_eq!(cluster_key(""), "");
        assert_eq!(cluster_key("src"), "src");
        assert_eq!(cluster_key("a/b/c"), "a/b/c");
        assert_eq!(cluster_key("a/b/c/d/e"), "a/b/c");
        // Subdirectories below the key share their parent's ring position.
        assert_eq!(
            cluster_center("crates/g/src/layout"),
            cluster_center("crates/g/src")
        );
    }

    #[test]
    fn depth_layers_are_fifty_units_apart_and_never_interleave() {
        for i in 0..500 {
            let id = format!("n{i}");
            let z0 = z_for(&id, 0);
            let z1 = z_for(&id, 1);
            assert!((z0 - z1 - Z_DEPTH_SPACING).abs() < 1e-3);
            assert!(z0.abs() <= Z_JITTER);
        }
    }

    #[test]
    fn entry_points_sit_on_top_and_callees_below() {
        // 0 -> 1 -> 2, 3 isolated.
        let depth = call_depths(4, &[(0, 1), (1, 2)]).expect("depths");
        assert_eq!(depth, vec![0, 1, 2, 0]);
        let top = position("src", "main", depth[0], 4);
        let leaf = position("src", "leaf", depth[2], 4);
        assert!(top.z > leaf.z);
    }

    #[test]
    fn depth_is_the_shortest_call_path() {
        // 0 -> 1 -> 2 -> 3 and 0 -> 3: node 3 is one call from the entry.
        let depth = call_depths(4, &[(0, 1), (1, 2), (2, 3), (0, 3)]).expect("depths");
        assert_eq!(depth, vec![0, 1, 2, 1]);
    }

    #[test]
    fn recursion_does_not_flatten_the_subtree_below_it() {
        // 0 calls itself and 1; 1 and 2 recurse mutually; 2 calls 3. Under a
        // zero-in-degree rule 0 has a caller (itself), nothing is a root, and
        // every node would fall back to depth 0.
        let calls = [(0, 0), (0, 1), (1, 2), (2, 1), (2, 3)];
        let depth = call_depths(4, &calls).expect("depths");
        assert_eq!(depth, vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_cycle_nobody_calls_is_a_root() {
        let depth = call_depths(3, &[(0, 1), (1, 0), (1, 2)]).expect("depths");
        assert_eq!(depth, vec![0, 0, 1]);
    }

    #[test]
    fn a_long_call_chain_does_not_overflow_the_stack() {
        let n = 200_000u32;
        let calls: Vec<(u32, u32)> = (0..n - 1).map(|i| (i, i + 1)).collect();
        let depth = call_depths(n as usize, &calls).expect("depths");
        assert_eq!(depth[n as usize - 1], n - 1);
        // And the same chain closed into one cycle is one root component.
        let mut ring = calls.clone();
        ring.push((n - 1, 0));
        let depth = call_depths(n as usize, &ring).expect("depths");
        assert!(depth.iter().all(|&d| d == 0));
    }

    #[test]
    fn an_edge_outside_the_node_range_raises() {
        assert!(matches!(
            call_depths(2, &[(0, 5)]),
            Err(LayoutError::EdgeOutOfRange {
                src: 0,
                dst: 5,
                node_count: 2
            })
        ));
    }

    /// A small deterministic call graph generator for the stability property.
    fn random_calls(n: u32, per_node: u32, seed: u64) -> Vec<(u32, u32)> {
        let mut state = seed;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u32
        };
        let mut calls = Vec::new();
        for src in 0..n {
            for _ in 0..per_node {
                calls.push((src, next() % n));
            }
        }
        calls
    }

    fn reachable_from(n: usize, calls: &[(u32, u32)], start: u32) -> Vec<bool> {
        let g = Csr::build(n, calls).expect("csr");
        let mut seen = vec![false; n];
        let mut stack = vec![start];
        while let Some(v) = stack.pop() {
            if !std::mem::replace(&mut seen[v as usize], true) {
                stack.extend_from_slice(g.targets(v));
            }
        }
        seen
    }

    #[test]
    fn inserting_a_node_changes_z_only_downstream_of_it() {
        // The Z stability rule, checked over many random graphs and inserts:
        // a node's Z changes only if its own depth changes, and its depth
        // changes only if it is reachable from the inserted node.
        for seed in 0..40u64 {
            let n = 400u32;
            let calls = random_calls(n, 1, seed);
            let before = call_depths(n as usize, &calls).expect("before");

            // The newcomer calls two existing nodes and is called by one.
            let u = n;
            let mut grown = calls.clone();
            let extra = random_calls(n, 1, seed + 1000);
            grown.push((u, extra[0].1));
            grown.push((u, extra[1].1));
            grown.push((extra[2].0, u));
            let after = call_depths(n as usize + 1, &grown).expect("after");
            let downstream = reachable_from(n as usize + 1, &grown, u);

            for v in 0..n as usize {
                let id = format!("node{v}");
                let z_before = z_for(&id, before[v]);
                let z_after = z_for(&id, after[v]);
                if !downstream[v] {
                    assert_eq!(before[v], after[v], "seed {seed}: node {v} depth moved");
                    assert_eq!(
                        z_before.to_bits(),
                        z_after.to_bits(),
                        "seed {seed}: node {v} is not downstream of the insert but its Z moved"
                    );
                }
                assert_eq!(
                    before[v] == after[v],
                    z_before.to_bits() == z_after.to_bits(),
                    "seed {seed}: node {v} Z changed without its depth changing, or the reverse"
                );
            }
        }
    }

    #[test]
    fn an_unconnected_insert_moves_nothing_at_all() {
        let n = 1000u32;
        let nodes: Vec<(String, String)> = (0..n)
            .map(|i| (format!("node{i}"), format!("crates/c{}/src", i % 25)))
            .collect();
        let edges: Vec<LayoutEdge> = random_calls(n, 2, 7)
            .into_iter()
            .map(|(src, dst)| LayoutEdge {
                src,
                dst,
                kind: EdgeKind::Calls,
            })
            .collect();
        fn view(nodes: &[(String, String)]) -> Vec<LayoutNode<'_>> {
            nodes
                .iter()
                .map(|(id, dir)| LayoutNode {
                    id,
                    directory: dir,
                    kind: NodeKind::Function,
                })
                .collect()
        }
        let before = layout_graph(&view(&nodes), &edges).expect("before");

        // A newcomer in another directory that calls nothing reaches nothing,
        // so no existing node may move by even one bit.
        let mut grown = nodes.clone();
        grown.push(("newcomer".into(), "far/away".into()));
        let after = layout_graph(&view(&grown), &edges).expect("after");
        for i in 0..n as usize {
            assert_eq!(before[i].position, after[i].position, "node {i} moved");
        }
    }

    #[test]
    fn layout_graph_agrees_with_the_cached_derivation() {
        let nodes = [
            LayoutNode {
                id: "a",
                directory: "src",
                kind: NodeKind::File,
            },
            LayoutNode {
                id: "b",
                directory: "src",
                kind: NodeKind::Function,
            },
        ];
        let edges = [
            LayoutEdge {
                src: 0,
                dst: 1,
                kind: EdgeKind::Contains,
            },
            LayoutEdge {
                src: 0,
                dst: 1,
                kind: EdgeKind::Calls,
            },
        ];
        let out = layout_graph(&nodes, &edges).expect("layout");
        assert_eq!(out[0].position, position("src", "a", 0, 2));
        assert_eq!(out[1].position, position("src", "b", 1, 2));
        assert_eq!(out[0].position, position_for("src", "a"));
        assert_eq!(out[1].degree, 2);
        assert_eq!(out[0].size, stellar::node_size(NodeKind::File, 2));
    }

    #[test]
    fn cluster_radius_grows_by_power_of_two_bucket() {
        assert_eq!(cluster_radius(0), CLUSTER_RADIUS);
        assert_eq!(cluster_radius(1), CLUSTER_RADIUS);
        assert_eq!(cluster_radius(64), CLUSTER_RADIUS);
        assert!((cluster_radius(65) - 5.0 * 128f32.sqrt()).abs() < 1e-4);
        assert_eq!(cluster_radius(65), cluster_radius(128));
        assert_eq!(cluster_radius(2_000), cluster_radius(2_048));
        assert!(cluster_radius(2_049) > cluster_radius(2_048));
        assert_eq!(cluster_radius(usize::MAX), cluster_radius(usize::MAX - 1));
    }

    /// Lay out `sizes[k]` nodes in directory `pkg{k}/src`, no edges.
    fn clusters(sizes: &[usize]) -> Vec<(String, String)> {
        sizes
            .iter()
            .enumerate()
            .flat_map(|(k, &n)| (0..n).map(move |i| (format!("c{k}n{i}"), format!("pkg{k}/src"))))
            .collect()
    }

    fn lay(nodes: &[(String, String)]) -> Vec<NodeVisual> {
        let view: Vec<LayoutNode<'_>> = nodes
            .iter()
            .map(|(id, dir)| LayoutNode {
                id,
                directory: dir,
                kind: NodeKind::Function,
            })
            .collect();
        layout_graph(&view, &[]).expect("layout")
    }

    #[test]
    fn an_insert_inside_a_bucket_moves_no_cluster_member() {
        let mut nodes = clusters(&[100, 30]);
        let before = lay(&nodes);
        nodes.push(("c0-new".into(), "pkg0/src".into()));
        let after = lay(&nodes);
        for i in 0..130 {
            assert_eq!(before[i].position, after[i].position, "node {i} moved");
        }
    }

    #[test]
    fn crossing_a_bucket_rescales_only_that_cluster_about_its_center() {
        // 64 -> 65 nodes crosses into the 128 bucket: radius 40 -> 56.6.
        let mut nodes = clusters(&[64, 30]);
        let before = lay(&nodes);
        nodes.push(("c0-new".into(), "pkg0/src".into()));
        let after = lay(&nodes);
        let (cx, cy) = cluster_center("pkg0/src");
        let scale = cluster_radius(65) / cluster_radius(64);
        let mut max_move = 0f32;
        for i in 0..64 {
            let (b, a) = (before[i].position, after[i].position);
            assert!((a.x - cx - (b.x - cx) * scale).abs() < 1e-3, "node {i}");
            assert!((a.y - cy - (b.y - cy) * scale).abs() < 1e-3, "node {i}");
            assert_eq!(a.z, b.z, "a rescale never moves Z");
            max_move = max_move.max(((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt());
        }
        // At most the rim moves: 40 * (sqrt 2 - 1) = 16.6 units.
        assert!(
            max_move <= CLUSTER_RADIUS * (scale - 1.0) + 1e-3,
            "{max_move}"
        );
        for i in 64..94 {
            assert_eq!(before[i].position, after[i].position, "other cluster moved");
        }
    }
}
