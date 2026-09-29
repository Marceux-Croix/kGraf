//! Render budget per path (decision 0054).
//!
//! The shell classifies the adapter (`kgraf_shell::adapter::classify`); the caller
//! maps that to a budget. The budget is an explicit input, never inferred here, so a
//! test can exercise the CPU budget on any adapter.

/// What the renderer may spend per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderBudget {
    /// Hardware adapter: every node, all edges, shaded cores with a glow halo.
    Gpu,
    /// Software rasterizer (0054): point sprites only (no halo pass), fewer of them
    /// (at most [`CPU_NODE_CAP`] individually, the rest grouped), edges only for a
    /// selection, a tighter on-screen size clamp, and no animation when nothing changed.
    Cpu,
}

/// Most nodes drawn individually on the CPU budget. Chosen, not measured: the
/// target is "10,000 nodes navigable in CPU mode", so a 10,000-node graph is drawn whole
/// and larger graphs group their lowest-degree nodes. To be tuned by the first
/// software-rasterizer measurement.
pub const CPU_NODE_CAP: usize = 10_000;

/// Grid cells per axis for grouping the nodes beyond the cap (at most 16^3 = 4,096
/// group sprites, whatever the graph size).
pub const CPU_GROUP_GRID: u32 = 16;

impl RenderBudget {
    /// Edges drawn with nothing selected.
    pub fn edges_without_selection(self) -> bool {
        matches!(self, Self::Gpu)
    }

    /// Whether the glow halo pass runs. The halo is where a software rasterizer spends
    /// its fill, so the CPU budget draws cores only.
    pub fn halos(self) -> bool {
        matches!(self, Self::Gpu)
    }

    /// Halo quad half-extent as a multiple of the core radius.
    pub fn halo_extent(self) -> f32 {
        2.5
    }

    /// Sphere-like shading on the sprite core (GPU only; one sqrt per fragment).
    pub fn shaded(self) -> bool {
        matches!(self, Self::Gpu)
    }

    /// Largest on-screen core radius in pixels. Zooming close to a node must not turn
    /// it, or dozens of its neighbors, into full-screen quads.
    pub fn max_core_px(self) -> f32 {
        match self {
            Self::Gpu => 160.0,
            Self::Cpu => 48.0,
        }
    }

    /// Nodes drawn individually before the rest are grouped, if capped.
    pub fn node_cap(self) -> Option<usize> {
        match self {
            Self::Gpu => None,
            Self::Cpu => Some(CPU_NODE_CAP),
        }
    }
}
