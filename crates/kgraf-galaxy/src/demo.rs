//! A synthetic scene shaped like the reference galaxy, for renderer tests and the
//! frame benchmark, where a scene of an exact size and shape is needed without an
//! index. The shell draws real projects ([`crate::project`]), never this.
//!
//! Dots follow the project view's rule (style guide STG-DRAW-2): every dot is the
//! palette's neutral code-dot color, and its size comes from
//! `kgraf_graph::layout::stellar::node_size` (kind and number of connections), the
//! same function [`crate::project`] calls. Edges use the palette's neutral edge color.
//! Color never carries degree or cluster. The positions are this module's own
//! clustered ring, not the layout's.

use kgraf_graph::NodeKind;
use kgraf_graph::layout::stellar::node_size;

use crate::palette::Palette;
use crate::scene::{EdgeInput, NodeInput, SceneInput};

/// Deterministic generator (splitmix64); the demo must look the same on every launch.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Roughly normal, mean 0, sd about 1 (sum of three uniforms).
    fn bell(&mut self) -> f32 {
        (self.unit() + self.unit() + self.unit() - 1.5) * 2.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
}

/// A clustered ring galaxy of `n` nodes with about two edges per node: clusters seeded
/// on a 500 to 750 ring, depth levels 50 apart on Z, 90% of edges inside a cluster at
/// intensity 0.25 and the rest across at 0.06 (the reference design's values).
pub fn synthetic(n: usize, seed: u64) -> SceneInput {
    let mut rng = Rng(seed);
    let clusters = ((n as f32).sqrt() / 4.0).round().max(1.0) as usize;
    let per = n.div_ceil(clusters).max(1);
    // The evidence's 500 to 750 ring, widened past 10,000 nodes so the demo's clusters
    // stay separated; the real layout makes its own choice.
    let ring = (n as f32 / 10_000.0).sqrt().max(1.0);
    let mut positions = Vec::with_capacity(n);
    let mut cluster_of = Vec::with_capacity(n);
    let mut kinds = Vec::with_capacity(n);
    for c in 0..clusters {
        let angle = rng.unit() * std::f32::consts::TAU;
        let radius = (500.0 + rng.unit() * 250.0) * ring;
        let z = -50.0 * rng.below(6) as f32;
        let center = [radius * angle.cos(), radius * angle.sin(), z];
        let sigma = 25.0 + (per as f32).sqrt() * 1.2;
        for _ in 0..per {
            if positions.len() == n {
                break;
            }
            positions.push([
                center[0] + rng.bell() * sigma,
                center[1] + rng.bell() * sigma,
                center[2] + rng.bell() * sigma * 0.5,
            ]);
            cluster_of.push(c);
            // Mostly functions, some files, few modules.
            let roll = rng.below(400);
            kinds.push(match roll {
                0 => NodeKind::Module,
                1..=9 => NodeKind::File,
                _ => NodeKind::Function,
            });
        }
    }
    let n = positions.len();
    let palette = Palette::default();
    let mut edges = Vec::with_capacity(n * 2);
    let mut degree = vec![0u32; n];
    for i in 0..n {
        let k = 1 + rng.below(3);
        for _ in 0..k {
            let cross = rng.below(10) == 0;
            let j = if cross {
                rng.below(n)
            } else {
                let c = cluster_of[i];
                let start = c * per;
                let len = per.min(n - start);
                start + rng.below(len)
            };
            if j == i {
                continue;
            }
            let same = cluster_of[i] == cluster_of[j];
            edges.push(EdgeInput {
                source: i as u32,
                target: j as u32,
                color: palette.edge,
                intensity: if same { 0.25 } else { 0.06 },
            });
            degree[i] += 1;
            degree[j] += 1;
        }
    }
    let nodes = positions
        .into_iter()
        .zip(kinds)
        .zip(&degree)
        .map(|((position, kind), &d)| NodeInput {
            position,
            color: palette.code_dot,
            size: node_size(kind, d),
        })
        .collect();
    SceneInput { nodes, edges }
}

/// A synthetic project snapshot of about `n` code nodes with a real code hierarchy, for
/// the fold tests and benchmark (decision 0068), where [`synthetic`]'s flat scene has
/// no parents. Directories of 5 to 40 files; each file holds 3 to 30 items: free
/// functions (leaves), `impl` blocks of 2 to 15 methods, and now and then a module of
/// functions, some of which hold a nested constant (depth 3). Every child has one
/// `contains` edge from its parent, as the indexer writes them; each function calls two
/// others, nine in ten within its directory. Positions come from the layout's pure
/// function, so clusters sit where the real layout puts them.
pub fn synthetic_project(n: usize, seed: u64) -> kgraf_graph::snapshot::GraphSnapshot {
    use kgraf_graph::layout::position;
    use kgraf_graph::snapshot::{GraphSnapshot, SceneEdge, SceneNode};
    use kgraf_graph::{EdgeKind, NodeKind};

    let mut rng = Rng(seed);
    let mut nodes: Vec<SceneNode> = Vec::with_capacity(n + 64);
    let mut edges: Vec<SceneEdge> = Vec::with_capacity(n * 3);
    // Function ids per directory, for calls.
    let mut fns_by_dir: Vec<Vec<usize>> = Vec::new();
    let add = |nodes: &mut Vec<SceneNode>,
               edges: &mut Vec<SceneEdge>,
               dir: &str,
               path: &str,
               kind: NodeKind,
               parent: Option<usize>| {
        let i = nodes.len();
        let id = format!("syn-{i}");
        let p = position(dir, &id, 0, 1);
        nodes.push(SceneNode {
            id: id.clone(),
            path: path.to_owned(),
            kind,
            qualified_name: format!("{}::{}{i}", path, kind.as_str()),
            position: p,
        });
        if let Some(parent) = parent {
            edges.push(SceneEdge {
                id: format!("syn-c-{i}"),
                src: nodes[parent].id.clone(),
                dst: id,
                kind: EdgeKind::Contains,
            });
        }
        i
    };
    let mut d = 0usize;
    while nodes.len() < n {
        let dir = format!("pkg{}/mod{d}/src", d % 16);
        let mut fns = Vec::new();
        let files = 5 + rng.below(36);
        for f in 0..files {
            if nodes.len() >= n {
                break;
            }
            let path = format!("{dir}/f{f}.rs");
            let file = add(&mut nodes, &mut edges, &dir, &path, NodeKind::File, None);
            for _ in 0..3 + rng.below(28) {
                match rng.below(20) {
                    0 => {
                        let m = add(
                            &mut nodes,
                            &mut edges,
                            &dir,
                            &path,
                            NodeKind::Module,
                            Some(file),
                        );
                        for _ in 0..2 + rng.below(5) {
                            let g = add(
                                &mut nodes,
                                &mut edges,
                                &dir,
                                &path,
                                NodeKind::Function,
                                Some(m),
                            );
                            fns.push(g);
                            if rng.below(3) == 0 {
                                add(
                                    &mut nodes,
                                    &mut edges,
                                    &dir,
                                    &path,
                                    NodeKind::Constant,
                                    Some(g),
                                );
                            }
                        }
                    }
                    1..=6 => {
                        let im = add(
                            &mut nodes,
                            &mut edges,
                            &dir,
                            &path,
                            NodeKind::Impl,
                            Some(file),
                        );
                        for _ in 0..2 + rng.below(14) {
                            let m = add(
                                &mut nodes,
                                &mut edges,
                                &dir,
                                &path,
                                NodeKind::Method,
                                Some(im),
                            );
                            fns.push(m);
                        }
                    }
                    _ => {
                        let g = add(
                            &mut nodes,
                            &mut edges,
                            &dir,
                            &path,
                            NodeKind::Function,
                            Some(file),
                        );
                        fns.push(g);
                    }
                }
            }
        }
        fns_by_dir.push(fns);
        d += 1;
    }
    let dirs = fns_by_dir.len();
    for (di, fns) in fns_by_dir.iter().enumerate() {
        for &f in fns {
            for k in 0..2 {
                let pool = if rng.below(10) == 0 {
                    &fns_by_dir[rng.below(dirs)]
                } else {
                    &fns_by_dir[di]
                };
                if pool.is_empty() {
                    continue;
                }
                let t = pool[rng.below(pool.len())];
                if t == f {
                    continue;
                }
                edges.push(SceneEdge {
                    id: format!("syn-k-{f}-{k}"),
                    src: nodes[f].id.clone(),
                    dst: nodes[t].id.clone(),
                    kind: EdgeKind::Calls,
                });
            }
        }
    }
    GraphSnapshot {
        nodes,
        edges,
        ..GraphSnapshot::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;

    #[test]
    fn synthetic_scene_is_valid_deterministic_and_sized() {
        let a = synthetic(10_000, 7);
        assert_eq!(a.nodes.len(), 10_000);
        assert!(
            a.edges.len() > 15_000 && a.edges.len() < 25_000,
            "{}",
            a.edges.len()
        );
        assert_eq!(a, synthetic(10_000, 7));
        Scene::new(a).expect("valid scene");
    }

    #[test]
    fn demo_dots_carry_no_category_or_degree_color() {
        let palette = Palette::default();
        let scene = synthetic(2_000, 7);
        // Sizes still vary with connections, so a uniform color is not a uniform scene.
        let sizes: std::collections::HashSet<u32> =
            scene.nodes.iter().map(|n| n.size.to_bits()).collect();
        assert!(sizes.len() > 3, "sizes should vary with kind and degree");
        for n in &scene.nodes {
            assert_eq!(n.color, palette.code_dot);
        }
        for e in &scene.edges {
            assert_eq!(e.color, palette.edge);
        }
    }
}
