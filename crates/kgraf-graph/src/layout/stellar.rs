//! Stellar visual language: color and size by degree and kind.
//!
//! Ported from codebase-memory-mcp `src/ui/layout3d.c` (`stellar_color`,
//! `size_for_label`, and the degree boost in `cbm_layout_compute`), MIT,
//! Copyright (c) 2025 DeusData, commit `64c23fab6c3eba3de226355c0ca1ff750116edab`.
//! Notice in `third_party/codebase-memory-mcp/LICENSE`.
//!
//! Degree owns color and size, so hubs are bigger, bluer stars. Kind colors
//! belong in text surfaces only, never in the scene (upstream's rule, kept).
//! Agents need a separate visual channel; they must not reuse these tables.

use crate::chunk::NodeKind;

/// Degree thresholds and colors, following the Hertzsprung-Russell sequence:
/// leaves are red dwarfs, mega-hubs blue giants. `(max degree, 0xRRGGBB)`.
pub const STELLAR_TABLE: [(u32, u32); 9] = [
    (1, 0xff6050),  // M, red dwarf
    (3, 0xff8855),  // late K, orange-red
    (5, 0xffa060),  // K, orange
    (8, 0xffc070),  // early K, warm orange
    (12, 0xffe080), // G, yellow (Sun-like)
    (18, 0xfff0c0), // F, yellow-white
    (25, 0xfff8e8), // late A, warm white
    (35, 0xe8e8ff), // A, white-blue
    (50, 0xc0d0ff), // B, blue-white
];
/// Above the last threshold: O, blue giant.
pub const STELLAR_HUB: u32 = 0x80a0ff;

/// Degree above which a node grows (upstream `deg > 5`).
pub const SIZE_BOOST_FROM_DEGREE: u32 = 5;
/// Size added per unit of degree once boosted (upstream `0.3f`).
pub const SIZE_BOOST_PER_DEGREE: f32 = 0.3;
/// Cap on the degree boost (upstream `10.0f`).
pub const SIZE_BOOST_MAX: f32 = 10.0;

/// Upstream base sizes for labels kGraf has no node kind for yet. Kept so
/// project, package and folder nodes, when they arrive, use the reference scale.
pub const PROJECT_SIZE: f32 = 20.0;
pub const PACKAGE_SIZE: f32 = 15.0;
pub const FOLDER_SIZE: f32 = 12.0;

/// The star color for a degree, as `0xRRGGBB`.
pub fn stellar_color(degree: u32) -> u32 {
    STELLAR_TABLE
        .iter()
        .find(|(max, _)| degree <= *max)
        .map_or(STELLAR_HUB, |(_, color)| *color)
}

/// Base size by kind, before the degree boost.
///
/// Upstream's table maps Module 15, File 8, Class, Struct and Interface 6,
/// Function and Method 4, anything else 4. `Trait` takes Interface's 6 because
/// upstream has no Trait label and a trait is Rust's interface.
pub fn base_size(kind: NodeKind) -> f32 {
    match kind {
        NodeKind::Module => 15.0,
        NodeKind::File => 8.0,
        NodeKind::Class | NodeKind::Struct | NodeKind::Interface | NodeKind::Trait => 6.0,
        NodeKind::Function
        | NodeKind::Method
        | NodeKind::Enum
        | NodeKind::Impl
        | NodeKind::Macro
        | NodeKind::TypeAlias
        | NodeKind::Constant
        | NodeKind::Decorated
        | NodeKind::Section => 4.0,
    }
}

/// The size boost a degree earns: none up to 5, then 0.3 per degree, capped
/// at 10.
pub fn degree_boost(degree: u32) -> f32 {
    if degree > SIZE_BOOST_FROM_DEGREE {
        (degree as f32 * SIZE_BOOST_PER_DEGREE).min(SIZE_BOOST_MAX)
    } else {
        0.0
    }
}

/// Visual size: base by kind plus the degree boost.
pub fn node_size(kind: NodeKind, degree: u32) -> f32 {
    base_size(kind) + degree_boost(degree)
}

/// A `0xRRGGBB` color as `[r, g, b]` channels in `0..=1`, the form
/// [`super::density::node_glow_boost`] takes.
pub fn channels(color: u32) -> [f32; 3] {
    [
        ((color >> 16) & 0xFF) as f32 / 255.0,
        ((color >> 8) & 0xFF) as f32 / 255.0,
        (color & 0xFF) as f32 / 255.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_follow_the_reference_thresholds() {
        // Each boundary on both sides, against upstream's literal values.
        let cases = [
            (0, 0xff6050),
            (1, 0xff6050),
            (2, 0xff8855),
            (3, 0xff8855),
            (4, 0xffa060),
            (5, 0xffa060),
            (6, 0xffc070),
            (8, 0xffc070),
            (9, 0xffe080),
            (12, 0xffe080),
            (13, 0xfff0c0),
            (18, 0xfff0c0),
            (19, 0xfff8e8),
            (25, 0xfff8e8),
            (26, 0xe8e8ff),
            (35, 0xe8e8ff),
            (36, 0xc0d0ff),
            (50, 0xc0d0ff),
            (51, 0x80a0ff),
            (10_000, 0x80a0ff),
        ];
        for (degree, color) in cases {
            assert_eq!(stellar_color(degree), color, "degree {degree}");
        }
    }

    #[test]
    fn hubs_are_bluer_than_leaves() {
        let [lr, _, lb] = channels(stellar_color(1));
        let [hr, _, hb] = channels(stellar_color(100));
        assert!(lr > lb, "a leaf is red-dominant");
        assert!(hb > hr, "a hub is blue-dominant");
    }

    #[test]
    fn sizes_follow_the_reference_table_and_boost() {
        assert_eq!(node_size(NodeKind::Module, 0), 15.0);
        assert_eq!(node_size(NodeKind::File, 0), 8.0);
        assert_eq!(node_size(NodeKind::Class, 5), 6.0);
        assert_eq!(node_size(NodeKind::Function, 5), 4.0);
        // Boost starts above 5: 6 * 0.3 = 1.8.
        assert!((node_size(NodeKind::Function, 6) - 5.8).abs() < 1e-6);
        // Capped at 10 from degree 34 on.
        assert_eq!(node_size(NodeKind::Method, 34), 14.0);
        assert_eq!(node_size(NodeKind::Method, 1000), 14.0);
    }

    #[test]
    fn size_never_shrinks_as_degree_grows() {
        let mut last = 0.0;
        for degree in 0..200 {
            let s = node_size(NodeKind::Function, degree);
            assert!(s >= last, "size fell at degree {degree}");
            last = s;
        }
    }
}
