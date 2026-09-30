//! Density rules: how bright edges draw as the scene grows, and the fixed
//! intensities and scales the renderer uses for edges and nodes.
//!
//! Ported from codebase-memory-mcp `graph-ui/src/lib/density.ts` and the
//! intensity and dimming rules in `graph-ui/src/components/EdgeLines.tsx` and
//! `NodeCloud.tsx`, MIT, Copyright (c) 2025 DeusData, commit
//! `64c23fab6c3eba3de226355c0ca1ff750116edab`. Notice in
//! `third_party/codebase-memory-mcp/LICENSE`.
//!
//! The failure these rules prevent: at scale the white blob is caused by edges,
//! not nodes. Edges blend additively and tens of thousands of long lines cross
//! the center, so their glow stacks into an opaque wash. Edges therefore dim on
//! an inverse-square-root curve that holds total glow roughly flat. A selection
//! is never density-scaled, so it stays bright against the dimmed rest.
//!
//! These are pure functions of counts; the renderer applies them.

/// Edge counts at or below this draw at full intensity.
pub const EDGE_REFERENCE_COUNT: usize = 2_500;
/// Floor on the edge scale, so a huge graph's edges never vanish.
pub const EDGE_MIN_SCALE: f32 = 0.05;
/// Edge intensity between two nodes of one cluster, with no selection.
pub const SAME_CLUSTER_EDGE: f32 = 0.25;
/// Edge intensity across clusters, with no selection.
pub const CROSS_CLUSTER_EDGE: f32 = 0.06;
/// Edge intensity when both endpoints are selected. Never density-scaled.
pub const SELECTED_EDGE: f32 = 0.5;
/// Edge intensity when one endpoint is selected, before density scaling.
pub const HALF_SELECTED_EDGE: f32 = 0.04;

/// Color multiplier for a node outside the selection.
pub const DIMMED_NODE_COLOR: f32 = 0.15;
/// Instance scale for a node in the selection, or when nothing is selected.
pub const NODE_SCALE: f32 = 0.5;
/// Instance scale for a node outside the selection.
pub const DIMMED_NODE_SCALE: f32 = 0.2;

/// Per-edge intensity multiplier for a scene with `edge_count` edges:
/// `1` up to 2,500, then `sqrt(2500 / n)`, floored at 0.05. Four times the
/// edges makes each about half as bright, so the total glow stays about flat.
pub fn edge_intensity_scale(edge_count: usize) -> f32 {
    if edge_count <= EDGE_REFERENCE_COUNT {
        return 1.0;
    }
    (EDGE_REFERENCE_COUNT as f32 / edge_count as f32)
        .sqrt()
        .max(EDGE_MIN_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn edge_scale_is_flat_then_inverse_square_root_then_floored() {
        assert_eq!(edge_intensity_scale(0), 1.0);
        assert_eq!(edge_intensity_scale(2_500), 1.0);
        assert!(close(edge_intensity_scale(10_000), 0.5));
        assert!(close(edge_intensity_scale(40_000), 0.25));
        // sqrt(2500 / n) = 0.05 at n = 1,000,000; the floor holds beyond.
        assert!(close(edge_intensity_scale(1_000_000), 0.05));
        assert_eq!(edge_intensity_scale(100_000_000), EDGE_MIN_SCALE);
    }

    #[test]
    fn total_edge_glow_stays_about_flat_as_edges_grow() {
        // The mechanism the curve exists for: n edges times the per-edge scale
        // grows as sqrt(n), not n, above the reference.
        let glow = |n: usize| n as f32 * edge_intensity_scale(n);
        let ratio = glow(40_000) / glow(10_000);
        assert!(close(ratio, 2.0), "4x edges gave {ratio}x total glow");
    }
}
