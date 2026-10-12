//! The galaxy renderer (decisions 0010, 0054, 0055, 0058): instanced wgpu
//! drawing of a project's graph, the fly-to camera and selection highlight, fed live
//! from the graph store's change log.
//!
//! - Input is baked layout data ([`scene::Scene`]); nothing here simulates forces.
//!   [`project`] builds it from a project's graph store: cached positions, gray
//!   dots sized by connections, cluster-aware edge intensities, plus agents and work items as
//!   their own populations. [`feed`] tails the change log on a background thread and
//!   [`project_view`] applies each batch in place, animated.
//! - Nodes are instanced camera-facing sprites: a depth-writing core and a glow halo;
//!   edges are additive one-pixel line segments with per-vertex color, depth tested
//!   but never writing depth. Code nodes are discs; agents diamonds; work items squares.
//! - [`budget::RenderBudget::Cpu`] is the software-rasterizer budget: cores only, at
//!   most [`budget::CPU_NODE_CAP`] nodes individually with the rest grouped, a tighter
//!   size clamp, and edges only for a selection.
//! - Frames render only on change ([`galaxy::Galaxy::needs_render`]).
//! - Large graphs fold (decision 0068, [`fold`]): children fold into their parents
//!   (chunks into files, files into directory cluster proxies) under a visible-node
//!   budget, a click expands a folded node, and notices on hidden children stack on
//!   the drawn ancestor as a badge ([`chunks`], [`notices`], [`badges`]).
//!
//! Provenance: shader techniques from rerun's `re_renderer` (MIT OR Apache-2.0,
//! `third_party/rerun-re_renderer/UPSTREAM`); scene, camera, selection and density
//! rules ported from codebase-memory-mcp (MIT, `third_party/codebase-memory-mcp/UPSTREAM`).

pub mod badges;
pub mod budget;
pub mod camera;
pub mod chunks;
pub mod demo;
pub mod error;
pub mod feed;
pub mod fold;
pub mod galaxy;
pub mod gpu;
pub mod labels;
pub mod lod;
pub mod notices;
pub mod palette;
pub mod pick;
pub mod plan;
pub mod project;
pub mod project_view;
pub mod scene;
pub mod selection;
pub mod sessions;
pub mod tree;
pub mod view;

pub use budget::RenderBudget;
pub use chunks::Chunks;
pub use error::GalaxyError;
pub use galaxy::Galaxy;
pub use palette::Palette;
pub use plan::{BlockKind, PlanNode, WorkState};
pub use project::ProjectModel;
pub use project_view::ProjectView;
pub use scene::{EdgeInput, NodeInput, Population, Scene, SceneInput};
pub use sessions::SessionOverlay;
pub use view::GalaxyView;
