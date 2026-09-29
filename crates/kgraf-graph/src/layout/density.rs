//! Density rules: how bright edges and nodes draw as the scene grows.
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
//! an inverse-square-root curve that holds total glow roughly flat, while node
//! glow and bloom stay at full strength to 25,000 nodes and only ease toward
//! floors by 250,000. A selection is never density-scaled, so it stays bright
//! against the dimmed rest.
//!
//! These are pure functions of counts and colors; the renderer applies them.

/// Edge counts at or below this draw at full intensity.
pub const EDGE_REFERENCE_COUNT: usize = 2_500;
/// Floor on the edge scale, so a huge graph's edges never vanish.
pub const EDGE_MIN_SCALE: f32 = 0.05;
/// Node glow and bloom are at full strength up to this many nodes.
pub const NODE_REFERENCE_COUNT: usize = 25_000;
/// Node glow and bloom reach their floors at this many nodes.
pub const NODE_FADE_END: usize = 250_000;
/// Bloom intensity floor.
pub const BLOOM_FLOOR: f32 = 0.7;
/// Node glow-boost floor.
pub const NODE_BOOST_FLOOR: f32 = 0.8;

/// Glow multiplier base and channel gains ([`node_glow_boost`]).
pub const GLOW_BASE: f32 = 1.35;
pub const GLOW_BLUE_GAIN: f32 = 2.4;
pub const GLOW_RED_GAIN: f32 = 0.9;

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

/// How far past the reference the node count is, in `0..=1`.
fn fade_factor(node_count: usize) -> f32 {
    if node_count <= NODE_REFERENCE_COUNT {
        return 0.0;
    }
    ((node_count - NODE_REFERENCE_COUNT) as f32 / (NODE_FADE_END - NODE_REFERENCE_COUNT) as f32)
        .min(1.0)
}

/// Bloom intensity multiplier for a scene of `node_count` nodes.
pub fn bloom_intensity_scale(node_count: usize) -> f32 {
    1.0 - fade_factor(node_count) * (1.0 - BLOOM_FLOOR)
}

/// Node glow-boost multiplier for a scene of `node_count` nodes.
pub fn node_boost_scale(node_count: usize) -> f32 {
    1.0 - fade_factor(node_count) * (1.0 - NODE_BOOST_FLOOR)
}

/// Color-aware glow multiplier, applied to a node before bloom.
///
/// Bloom is luminance-thresholded and blue has almost no luminance weight, so a
/// brightness-based boost blows out white and yellow nodes and leaves the blue
/// hubs flat. The boost goes by channel dominance instead: blue-dominant nodes
/// most, red-dominant modestly, white and yellow least. Channels are `0..=1`;
/// the result is at least [`GLOW_BASE`].
pub fn node_glow_boost(r: f32, g: f32, b: f32) -> f32 {
    let blueness = (b - r.max(g)).max(0.0);
    let redness = (r - g.max(b)).max(0.0);
    GLOW_BASE + blueness * GLOW_BLUE_GAIN + redness * GLOW_RED_GAIN
}

/// Where an edge stands against the current selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeSelection {
    /// Nothing is selected.
    NoSelection,
    /// Both endpoints are selected.
    Both,
    /// Exactly one endpoint is selected.
    One,
    /// Something is selected, but neither endpoint.
    Neither,
}

/// An edge's intensity, or `None` when it is not drawn at all.
///
/// With no selection: 0.25 inside a cluster, 0.06 across, both times the
/// density scale. With a selection: 0.5 for a selected edge (never scaled),
/// 0.04 times the density scale for a half-selected one, and edges touching no
/// selected node are dropped.
pub fn edge_intensity(
    same_cluster: bool,
    selection: EdgeSelection,
    density_scale: f32,
) -> Option<f32> {
    match selection {
        EdgeSelection::NoSelection => {
            let base = if same_cluster {
                SAME_CLUSTER_EDGE
            } else {
                CROSS_CLUSTER_EDGE
            };
            Some(base * density_scale)
        }
        EdgeSelection::Both => Some(SELECTED_EDGE),
        EdgeSelection::One => Some(HALF_SELECTED_EDGE * density_scale),
        EdgeSelection::Neither => None,
    }
}

/// Whether two nodes' directories fall in one cluster, for [`edge_intensity`].
pub fn same_cluster(directory_a: &str, directory_b: &str) -> bool {
    super::cluster_key(directory_a) == super::cluster_key(directory_b)
}

/// A node's draw color `[r, g, b]` and instance scale multiplier.
///
/// Outside a selection: color times 0.15, scale 0.2. Otherwise the color is
/// boosted above 1 by [`node_glow_boost`], faded toward flat by `boost_scale`
/// ([`node_boost_scale`]), so bloom picks up the excess as a corona.
pub fn node_appearance(color: u32, dimmed: bool, boost_scale: f32) -> ([f32; 3], f32) {
    let [r, g, b] = super::stellar::channels(color);
    if dimmed {
        return (
            [
                r * DIMMED_NODE_COLOR,
                g * DIMMED_NODE_COLOR,
                b * DIMMED_NODE_COLOR,
            ],
            DIMMED_NODE_SCALE,
        );
    }
    let applied = 1.0 + (node_glow_boost(r, g, b) - 1.0) * boost_scale;
    ([r * applied, g * applied, b * applied], NODE_SCALE)
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

    #[test]
    fn node_scales_hold_then_ease_to_their_floors() {
        assert_eq!(bloom_intensity_scale(25_000), 1.0);
        assert_eq!(node_boost_scale(25_000), 1.0);
        assert!(close(bloom_intensity_scale(137_500), 0.85));
        assert!(close(node_boost_scale(137_500), 0.9));
        assert!(close(bloom_intensity_scale(250_000), BLOOM_FLOOR));
        assert!(close(node_boost_scale(10_000_000), NODE_BOOST_FLOOR));
    }

    #[test]
    fn blue_hubs_glow_most_and_white_least() {
        let [r, g, b] = super::super::stellar::channels(0x80a0ff);
        let hub = node_glow_boost(r, g, b);
        let [r, g, b] = super::super::stellar::channels(0xff6050);
        let leaf = node_glow_boost(r, g, b);
        let [r, g, b] = super::super::stellar::channels(0xfff8e8);
        let white = node_glow_boost(r, g, b);
        assert!(
            hub > leaf && leaf > white,
            "hub {hub} leaf {leaf} white {white}"
        );
        assert!(close(node_glow_boost(1.0, 1.0, 1.0), GLOW_BASE));
        assert!(close(
            node_glow_boost(0.0, 0.0, 1.0),
            GLOW_BASE + GLOW_BLUE_GAIN
        ));
    }

    #[test]
    fn edge_intensity_follows_cluster_and_selection() {
        assert_eq!(
            edge_intensity(true, EdgeSelection::NoSelection, 1.0),
            Some(0.25)
        );
        assert_eq!(
            edge_intensity(false, EdgeSelection::NoSelection, 1.0),
            Some(0.06)
        );
        assert_eq!(
            edge_intensity(false, EdgeSelection::NoSelection, 0.5),
            Some(0.03)
        );
        // A selected edge ignores density; a half-selected one does not.
        assert_eq!(edge_intensity(false, EdgeSelection::Both, 0.05), Some(0.5));
        assert_eq!(edge_intensity(true, EdgeSelection::One, 0.5), Some(0.02));
        assert_eq!(edge_intensity(true, EdgeSelection::Neither, 1.0), None);
    }

    #[test]
    fn same_cluster_uses_the_ring_key() {
        assert!(same_cluster("crates/g/src", "crates/g/src/layout"));
        assert!(!same_cluster("crates/g/src", "crates/h/src"));
    }

    #[test]
    fn dimmed_nodes_drop_to_fifteen_percent() {
        let (rgb, scale) = node_appearance(0xffffff, true, 1.0);
        assert!(rgb.iter().all(|c| close(*c, 0.15)));
        assert_eq!(scale, DIMMED_NODE_SCALE);
        // Undimmed white gets the base boost; with the boost faded out it is flat.
        let (rgb, scale) = node_appearance(0xffffff, false, 1.0);
        assert!(close(rgb[0], GLOW_BASE));
        assert_eq!(scale, NODE_SCALE);
        let (rgb, _) = node_appearance(0xffffff, false, 0.0);
        assert!(close(rgb[0], 1.0));
    }
}
