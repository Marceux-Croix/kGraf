//! Level of detail for the CPU budget (0054: "a lower cap on visible nodes before
//! aggregation").
//!
//! The highest-degree nodes are drawn individually up to the cap; ties go to the lower
//! index, so the choice is deterministic for a given scene. The rest are grouped by a
//! coarse grid over the scene's bounds, one sprite per occupied cell at the members'
//! centroid. Degree decides because hubs carry the structure the user navigates by, and
//! it is already the reference's size and color key. Written for kGraf: the
//! reference switches to points above 75,000 nodes but never aggregates.

use glam::Vec3;

use crate::scene::Scene;

/// A group sprite standing in for several hidden nodes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Group {
    pub position: [f32; 3],
    /// Mean sRGB color of the members.
    pub color: [f32; 3],
    /// Core radius: mean member size times sqrt(count), so area tracks membership.
    pub size: f32,
    pub count: u32,
}

/// Which nodes are drawn individually, and the groups for the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct Visibility {
    shown: Vec<bool>,
    groups: Vec<Group>,
    /// Per slot: the group standing for it (`u32::MAX` when shown or not grouped).
    group_of: Vec<u32>,
    /// No cap applies: every slot is shown, including one inserted after this was
    /// computed (liveness is the caller's check).
    everything: bool,
}

impl Visibility {
    /// Every live node shown, no groups.
    pub fn all(scene: &Scene) -> Self {
        Self {
            shown: (0..scene.nodes().len() as u32)
                .map(|i| scene.is_alive(i))
                .collect(),
            groups: Vec::new(),
            group_of: Vec::new(),
            everything: true,
        }
    }

    /// At most `cap` nodes shown, chosen by degree; the rest grouped on a `grid`^3 grid.
    /// Agents and work items are always shown and count against the cap first: they
    /// are few (bounded by plan size) and are what the user acts on. Nodes `folded`
    /// reports (hidden in a folded parent, decision 0068) are neither shown nor
    /// grouped: their parent stands in for them.
    pub fn capped(scene: &Scene, cap: usize, grid: u32, folded: impl Fn(u32) -> bool) -> Self {
        let nodes = scene.nodes();
        let n = nodes.len();
        let mut order: Vec<u32> = (0..n as u32)
            .filter(|&i| scene.is_alive(i) && !folded(i))
            .collect();
        if order.len() <= cap {
            return Self::all(scene);
        }
        // Overlay and cluster proxies first, then descending degree, then ascending
        // index: deterministic.
        order.sort_unstable_by_key(|&i| {
            (
                scene.population(i) == crate::scene::Population::Code,
                std::cmp::Reverse(scene.degree(i)),
                i,
            )
        });
        let mut shown = vec![false; n];
        for &i in &order[..cap] {
            shown[i as usize] = true;
        }

        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for &i in &order {
            let p = Vec3::from(nodes[i as usize].position);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        let g = grid.max(1);
        let cell = ((hi - lo) / g as f32).max(Vec3::splat(1e-3));
        let cell_of = |p: Vec3| {
            let c = ((p - lo) / cell)
                .floor()
                .as_uvec3()
                .min(glam::UVec3::splat(g - 1));
            ((c.z * g + c.y) * g + c.x) as usize
        };
        // Per cell: position sum, color sum, size sum, count. Local accumulation; one pass.
        let mut acc = vec![(Vec3::ZERO, Vec3::ZERO, 0.0f32, 0u32); (g * g * g) as usize];
        let mut cell = vec![u32::MAX; n];
        for &i in &order[cap..] {
            let node = &nodes[i as usize];
            let p = Vec3::from(node.position);
            let c = cell_of(p);
            cell[i as usize] = c as u32;
            let a = &mut acc[c];
            a.0 += p;
            a.1 += Vec3::from(node.color);
            a.2 += node.size;
            a.3 += 1;
        }
        // Cells to group indices (only occupied cells become groups).
        let mut index = vec![u32::MAX; acc.len()];
        let mut next = 0u32;
        for (k, a) in acc.iter().enumerate() {
            if a.3 > 0 {
                index[k] = next;
                next += 1;
            }
        }
        let group_of = cell
            .into_iter()
            .map(|c| if c == u32::MAX { c } else { index[c as usize] })
            .collect();
        let groups = acc
            .into_iter()
            .filter(|a| a.3 > 0)
            .map(|(p, c, s, count)| {
                let k = count as f32;
                Group {
                    position: (p / k).to_array(),
                    color: (c / k).to_array(),
                    size: s / k * k.sqrt(),
                    count,
                }
            })
            .collect();
        Self {
            shown,
            groups,
            group_of,
            everything: false,
        }
    }

    /// The group standing for `node`, when the cap grouped it.
    pub fn group_of(&self, node: u32) -> Option<u32> {
        self.group_of
            .get(node as usize)
            .copied()
            .filter(|&g| g != u32::MAX)
    }

    pub fn is_shown(&self, node: u32) -> bool {
        self.everything || self.shown.get(node as usize).copied().unwrap_or(false)
    }

    pub fn shown_count(&self) -> usize {
        self.shown.iter().filter(|&&s| s).count()
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{EdgeInput, NodeInput, SceneInput};

    /// A star: node 0 has degree `n - 1`, node 1 an extra edge, the rest degree 1.
    fn star(n: u32) -> Scene {
        let nodes = (0..n)
            .map(|i| NodeInput {
                position: [i as f32, (i % 7) as f32, 0.0],
                color: [1.0, 0.5, 0.25],
                size: 4.0,
            })
            .collect();
        let edge = |s, t| EdgeInput {
            source: s,
            target: t,
            color: [1.0; 3],
            intensity: 0.25,
        };
        let mut edges: Vec<_> = (1..n).map(|i| edge(0, i)).collect();
        edges.push(edge(1, 2));
        Scene::new(SceneInput { nodes, edges }).expect("valid")
    }

    #[test]
    fn under_the_cap_everything_is_shown() {
        let s = star(50);
        let v = Visibility::capped(&s, 50, 4, |_| false);
        assert_eq!(v.shown_count(), 50);
        assert!(v.groups().is_empty());
    }

    #[test]
    fn cap_keeps_highest_degree_then_lowest_index() {
        let s = star(1_000);
        let v = Visibility::capped(&s, 10, 4, |_| false);
        assert_eq!(v.shown_count(), 10);
        // Hub (999), then nodes 1 and 2 (degree 2), then 3..=9 by index.
        for i in 0..10 {
            assert!(v.is_shown(i), "node {i}");
        }
        assert!(!v.is_shown(10));
        assert_eq!(v, Visibility::capped(&s, 10, 4, |_| false), "deterministic");
    }

    #[test]
    fn folded_nodes_are_neither_shown_nor_grouped() {
        let s = star(1_000);
        // Fold away everything from 500 on: 500 remain, 10 shown, 490 grouped.
        let v = Visibility::capped(&s, 10, 4, |i| i >= 500);
        assert_eq!(v.shown_count(), 10);
        let grouped: u32 = v.groups().iter().map(|g| g.count).sum();
        assert_eq!(grouped, 490);
        // Folding below the cap draws all that remain, ungrouped.
        let v = Visibility::capped(&s, 10, 4, |i| i >= 10);
        assert!(v.groups().is_empty());
    }

    #[test]
    fn every_hidden_node_is_in_exactly_one_group() {
        let s = star(1_000);
        let v = Visibility::capped(&s, 10, 4, |_| false);
        let grouped: u32 = v.groups().iter().map(|g| g.count).sum();
        assert_eq!(grouped, 990);
        // Each hidden node maps to the group that counts it.
        let mut per = vec![0u32; v.groups().len()];
        for i in 0..1_000u32 {
            match v.group_of(i) {
                Some(g) => per[g as usize] += 1,
                None => assert!(v.is_shown(i), "node {i} neither shown nor grouped"),
            }
        }
        let counts: Vec<u32> = v.groups().iter().map(|g| g.count).collect();
        assert_eq!(per, counts);
        assert!(v.groups().len() <= 64);
        for g in v.groups() {
            assert_eq!(g.color, [1.0, 0.5, 0.25]);
            assert!((g.size - 4.0 * (g.count as f32).sqrt()).abs() < 1e-3);
        }
    }
}
