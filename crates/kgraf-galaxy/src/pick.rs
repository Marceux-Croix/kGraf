//! CPU picking in screen space.
//!
//! Written, not reused. The roadmap names `rstar` for picking; a 3D R-tree answers
//! nearest-in-world, but a click asks nearest-on-screen under a perspective camera,
//! which changes with every camera move. A linear projection pass runs only on click
//! and measured 0.25 ms at 100,000 nodes on an M4 Max (`benches/frame.rs`, `pick`), so
//! an index would add a dependency and a rebuild to save nothing a user can feel.
//! re_renderer's GPU picking layer needs a readback round trip of a frame or more;
//! rejected for the same reason.

use glam::Vec3;

use crate::camera::CameraFrame;
use crate::selection::NodeInstance;

/// Minimum on-screen core radius in pixels, shared with the sprite shader so a node
/// drawn at the minimum is also pickable at it.
pub const MIN_CORE_PX: f32 = 1.5;
/// Extra reach around a node's core, so tiny far nodes are clickable.
pub const PICK_SLOP_PX: f32 = 6.0;
/// A proxy ring's inner radius, in core radii: the shader draws nothing inside it
/// (`points.wgsl` `RING_INNER`), so the hollow has no pick area either and a node seen
/// through it is picked, not the ring (2026-09-27).
pub const RING_INNER: f32 = 0.62;

/// Whether `inst` is a proxy ring (shape 4, `scene::Population::shape`).
fn is_ring(inst: &NodeInstance) -> bool {
    inst.color[3] > 3.5
}

/// On-screen core radius in pixels of a node of world radius `size` at view `depth`,
/// clamped exactly as the sprite shader clamps it.
pub fn core_px(size: f32, depth: f32, pixel_world_size_per_depth: f32, max_core_px: f32) -> f32 {
    (size / (depth * pixel_world_size_per_depth)).clamp(MIN_CORE_PX, max_core_px)
}

/// The node under `cursor` (physical pixels, origin top left), if any, among nodes
/// `drawn` reports as drawn.
///
/// Ranking follows what the user sees with the depth buffer on:
/// 1. a node whose drawn core contains the cursor beats one that is merely within
///    reach, since the core is what occludes;
/// 2. among those, the nearer node wins (it is drawn in front);
/// 3. among nodes only within reach, the nearest center on screen wins, then depth.
pub fn pick(
    instances: &[NodeInstance],
    drawn: impl Fn(u32) -> bool,
    frame: &CameraFrame,
    cursor: [f32; 2],
    max_core_px: f32,
) -> Option<u32> {
    pick_detail(instances, drawn, frame, cursor, max_core_px).map(|(s, _)| s)
}

/// [`pick`], and whether the winner's drawn core contains the cursor (`false`: it is
/// only within reach).
pub fn pick_detail(
    instances: &[NodeInstance],
    drawn: impl Fn(u32) -> bool,
    frame: &CameraFrame,
    cursor: [f32; 2],
    max_core_px: f32,
) -> Option<(u32, bool)> {
    // (inside core, key1, key2, index): smaller keys win within a class.
    let mut best: Option<(bool, f32, f32, u32)> = None;
    for (i, inst) in instances.iter().enumerate() {
        // Scene::new bounds the node count to u32.
        let i = i as u32;
        if !drawn(i) {
            continue;
        }
        let [x, y, z, size] = inst.pos_size;
        let Some((px, depth)) = frame.project(Vec3::new(x, y, z)) else {
            continue;
        };
        let d = (px[0] - cursor[0]).hypot(px[1] - cursor[1]);
        let core = core_px(size, depth, frame.pixel_world_size_per_depth, max_core_px);
        if d > core + PICK_SLOP_PX || (is_ring(inst) && d < RING_INNER * core) {
            continue;
        }
        let inside = d <= core;
        let cand = if inside {
            (true, depth, d, i)
        } else {
            (false, d, depth, i)
        };
        let better = match best {
            None => true,
            Some(b) => match (cand.0, b.0) {
                (true, false) => true,
                (false, true) => false,
                _ => (cand.1, cand.2) < (b.1, b.2),
            },
        };
        if better {
            best = Some(cand);
        }
    }
    best.map(|b| (b.3, b.0))
}

/// Whether `cursor` is within reach of `inst`: its drawn core plus [`PICK_SLOP_PX`].
pub fn within_reach(
    inst: &NodeInstance,
    frame: &CameraFrame,
    cursor: [f32; 2],
    max_core_px: f32,
) -> bool {
    if inst.is_hidden() {
        return false;
    }
    let [x, y, z, size] = inst.pos_size;
    let Some((px, depth)) = frame.project(Vec3::new(x, y, z)) else {
        return false;
    };
    let d = (px[0] - cursor[0]).hypot(px[1] - cursor[1]);
    let core = core_px(size, depth, frame.pixel_world_size_per_depth, max_core_px);
    d <= core + PICK_SLOP_PX && !(is_ring(inst) && d < RING_INNER * core)
}

/// Whether `inst`'s drawn core covers `cursor` (the reach around it not included).
pub fn covers(
    inst: &NodeInstance,
    frame: &CameraFrame,
    cursor: [f32; 2],
    max_core_px: f32,
) -> bool {
    if inst.is_hidden() {
        return false;
    }
    let [x, y, z, size] = inst.pos_size;
    let Some((px, depth)) = frame.project(Vec3::new(x, y, z)) else {
        return false;
    };
    let d = (px[0] - cursor[0]).hypot(px[1] - cursor[1]);
    d <= core_px(size, depth, frame.pixel_world_size_per_depth, max_core_px)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::OrbitCamera;

    const MAX: f32 = 160.0;

    fn inst(x: f32, y: f32, z: f32, size: f32) -> NodeInstance {
        NodeInstance {
            pos_size: [x, y, z, size],
            color: [1.0; 4],
        }
    }

    fn all(_: u32) -> bool {
        true
    }

    #[test]
    fn picks_the_node_under_the_cursor() {
        let frame = OrbitCamera::default().frame(800.0, 600.0);
        let nodes = [inst(0.0, 0.0, 0.0, 10.0), inst(200.0, 0.0, 0.0, 10.0)];
        let (p1, _) = frame.project(Vec3::new(200.0, 0.0, 0.0)).expect("visible");
        assert_eq!(pick(&nodes, all, &frame, [400.0, 300.0], MAX), Some(0));
        assert_eq!(pick(&nodes, all, &frame, p1, MAX), Some(1));
        assert_eq!(pick(&nodes, all, &frame, [10.0, 10.0], MAX), None);
    }

    #[test]
    fn nearer_node_wins_a_tie() {
        let frame = OrbitCamera::default().frame(800.0, 600.0);
        // Both on the view axis: the one at z = 100 is nearer the eye at z = 800.
        let nodes = [inst(0.0, 0.0, 0.0, 10.0), inst(0.0, 0.0, 100.0, 10.0)];
        assert_eq!(pick(&nodes, all, &frame, [400.0, 300.0], MAX), Some(1));
    }

    #[test]
    fn a_large_near_core_over_the_cursor_beats_a_small_far_center() {
        let frame = OrbitCamera::default().frame(800.0, 600.0);
        // Node 0: small, far, centered exactly under the cursor. Node 1: large, near,
        // centered 30 px away, its drawn core covering the cursor and hiding node 0.
        let far = inst(0.0, 0.0, -200.0, 2.0);
        let world_30px = 30.0 * frame.pixel_world_size_per_depth * 500.0;
        let near = inst(world_30px, 0.0, 300.0, 60.0);
        let (p_near, d_near) = frame
            .project(Vec3::new(world_30px, 0.0, 300.0))
            .expect("visible");
        let core = core_px(60.0, d_near, frame.pixel_world_size_per_depth, MAX);
        assert!(
            (p_near[0] - 430.0).abs() < 1.0 && core > 30.0,
            "setup: {p_near:?} {core}"
        );
        assert_eq!(
            pick(&[far, near], all, &frame, [400.0, 300.0], MAX),
            Some(1)
        );
    }

    #[test]
    fn undrawn_nodes_and_nodes_behind_the_camera_are_not_picked() {
        let frame = OrbitCamera::default().frame(800.0, 600.0);
        let nodes = [inst(0.0, 0.0, 900.0, 10.0), inst(0.0, 0.0, 0.0, 10.0)];
        assert_eq!(pick(&nodes[..1], all, &frame, [400.0, 300.0], MAX), None);
        assert_eq!(pick(&nodes, |i| i != 1, &frame, [400.0, 300.0], MAX), None);
    }

    #[test]
    fn covers_is_the_drawn_core_only() {
        let frame = OrbitCamera::default().frame(800.0, 600.0);
        let n = inst(0.0, 0.0, 0.0, 10.0);
        let (_, depth) = frame.project(Vec3::ZERO).expect("visible");
        let r = core_px(10.0, depth, frame.pixel_world_size_per_depth, MAX);
        assert!(covers(&n, &frame, [400.0, 300.0], MAX));
        assert!(covers(&n, &frame, [400.0 + r - 0.1, 300.0], MAX));
        assert!(
            !covers(&n, &frame, [400.0 + r + 1.0, 300.0], MAX),
            "slop is not core"
        );
        assert!(!covers(&NodeInstance::HIDDEN, &frame, [400.0, 300.0], MAX));
        assert!(within_reach(
            &n,
            &frame,
            [400.0 + r + PICK_SLOP_PX - 0.5, 300.0],
            MAX
        ));
        assert!(!within_reach(
            &n,
            &frame,
            [400.0 + r + PICK_SLOP_PX + 1.0, 300.0],
            MAX
        ));
        let nodes = [n];
        assert_eq!(
            pick_detail(&nodes, all, &frame, [400.0, 300.0], MAX),
            Some((0, true))
        );
        assert_eq!(
            pick_detail(&nodes, all, &frame, [400.0 + r + 2.0, 300.0], MAX),
            Some((0, false))
        );
    }

    /// The ring's hollow is not the ring: a node behind it is picked there, and the
    /// ring only on its band.
    #[test]
    fn a_rings_hollow_has_no_pick_area() {
        let frame = OrbitCamera::default().frame(800.0, 600.0);
        let ring = NodeInstance {
            pos_size: [0.0, 0.0, 100.0, 40.0],
            color: [1.0, 1.0, 1.0, 4.0],
        };
        let behind = inst(0.0, 0.0, 0.0, 5.0);
        let nodes = [ring, behind];
        assert_eq!(
            pick(&nodes, all, &frame, [400.0, 300.0], MAX),
            Some(1),
            "through the hollow"
        );
        let (_, depth) = frame.project(Vec3::new(0.0, 0.0, 100.0)).expect("visible");
        let r = core_px(40.0, depth, frame.pixel_world_size_per_depth, MAX);
        assert_eq!(
            pick(&nodes, all, &frame, [400.0 + 0.8 * r, 300.0], MAX),
            Some(0),
            "on the band"
        );
        assert!(!within_reach(&ring, &frame, [400.0 + 0.3 * r, 300.0], MAX));
        assert!(within_reach(&ring, &frame, [400.0 + 0.8 * r, 300.0], MAX));
    }

    #[test]
    fn core_px_is_clamped_both_ways() {
        assert_eq!(core_px(1e-6, 800.0, 0.001, 48.0), MIN_CORE_PX);
        assert_eq!(core_px(1e6, 1.0, 0.001, 48.0), 48.0);
        assert!((core_px(8.0, 1000.0, 0.001, 48.0) - 8.0).abs() < 1e-4);
    }
}
