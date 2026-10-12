//! Dot size by kind and number of connections (style guide STG-DRAW-2: dots are
//! gray, so there is no color table here).
//!
//! Ported from codebase-memory-mcp `src/ui/layout3d.c` (`size_for_label` and the
//! degree boost in `cbm_layout_compute`), MIT,
//! Copyright (c) 2025 DeusData, commit `64c23fab6c3eba3de226355c0ca1ff750116edab`.
//! Notice in `third_party/codebase-memory-mcp/LICENSE`.
//!
//! Degree owns size, so hubs are bigger dots. Agents need a separate visual
//! channel; they must not reuse this table.

use crate::chunk::NodeKind;

/// Degree above which a node grows (upstream `deg > 5`).
pub const SIZE_BOOST_FROM_DEGREE: u32 = 5;
/// Size added per unit of degree once boosted (upstream `0.3f`).
pub const SIZE_BOOST_PER_DEGREE: f32 = 0.3;
/// Cap on the degree boost (upstream `10.0f`).
pub const SIZE_BOOST_MAX: f32 = 10.0;

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

#[cfg(test)]
mod tests {
    use super::*;

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
