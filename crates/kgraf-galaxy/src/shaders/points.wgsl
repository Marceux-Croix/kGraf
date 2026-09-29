// Node sprites: camera-facing quads per instance, drawn in two passes.
//   core pass (vs_core / fs_core): the disc, depth tested and depth writing, so a near
//     node covers a far one whatever the instance order;
//   halo pass (vs_halo / fs_halo): the radial-gradient glow outside the disc, depth
//     tested against the cores but not writing, premultiplied over.
//
// Adapted from rerun re_renderer (https://github.com/rerun-io/rerun, commit
// f54fa0212aa1ff8595ea60fe841fd385821c1644, MIT OR Apache-2.0, Copyright (c) 2022
// Rerun Technologies AB; see third_party/rerun-re_renderer/):
//   crates/viewer_support/re_renderer/shader/utils/sphere_quad.wgsl
//     - the six-vertex quad spanned from vertex_index with no index buffer
//       (top_bottom / left_right corner pattern of sphere_or_circle_quad_span);
//     - circle_quad: a camera-plane billboard plus half a pixel of feather margin;
//     - circle_quad_coverage: fwidth-feathered circle edge.
//   crates/viewer_support/re_renderer/shader/utils/size.wgsl
//     - world_size_from_point_size: a minimum size in pixels resolved to world units
//       at the point's camera depth.
//   crates/viewer_support/re_renderer/shader/point_cloud.wgsl
//     - the quick sphere-like shading term max(0.4, sqrt(1.2 - d / r)).
// Changed for kGraf: instance-rate vertex buffers instead of re_renderer's data
// textures (which exist for WebGL; every native wgpu backend including llvmpipe and
// WARP has instancing); a maximum on-screen size as well as a minimum; the core/halo
// split; no picking, outline or depth-offset variants.

struct Frame {
    view_proj: mat4x4f,
    eye: vec4f,
    forward: vec4f,
    right: vec4f,
    up: vec4f,
    // x: world size of one pixel at depth 1, y: minimum core radius in pixels,
    // z: halo half-extent in core radii, w: 1 for shaded cores, 0 for flat.
    params: vec4f,
    // x: maximum core radius in pixels, y: edge density scale (lines.wgsl).
    limits: vec4f,
    // rgb: a work card's fill (linear), from the palette.
    card: vec4f,
};

@group(0) @binding(0)
var<uniform> frame: Frame;

struct Instance {
    @location(0) pos_size: vec4f,
    // rgb linear color; a the shape: 0 free slot (nothing drawn), 1 code disc,
    // 2 actor disk with a border, 3 work-item card, 4 proxy ring
    // (scene::Population::shape).
    @location(1) color: vec4f,
};

struct VertexOut {
    @builtin(position) position: vec4f,
    @location(0) @interpolate(flat) color: vec3f,
    // Offset from the center in core radii, interpolated across the quad.
    @location(1) @interpolate(perspective) offset: vec2f,
    @location(2) @interpolate(flat) shape: f32,
};

// Halo brightness relative to the core at the core's edge.
const HALO_STRENGTH: f32 = 0.35;
// A proxy ring's inner radius, in core radii. pick::RING_INNER must match: the hollow
// has no pick area.
const RING_INNER: f32 = 0.62;
// An actor's border (style guide STG-DRAW-7): the outer part of its disk, in core
// radii, drawn as the fill mixed toward white at 60%.
const ACTOR_BORDER: f32 = 0.86;
const ACTOR_BORDER_MIX: f32 = 0.6;
// A work-item card (STG-DRAW-5): half its height over its half width, and its corner
// radius, in core radii. The fill is the palette's card color (frame.card); the border
// is the instance color, the card's state tint.
const CARD_HALF_HEIGHT: f32 = 0.68;
const CARD_CORNER: f32 = 0.3;
const CARD_BORDER: f32 = 0.14;

fn span(vertex_idx: u32, inst: Instance, extent: f32) -> VertexOut {
    var out: VertexOut;
    if inst.color.a < 0.5 {
        // A free slot: every corner outside the clip volume, so nothing rasterizes.
        out.position = vec4f(2.0, 2.0, 2.0, 1.0);
        out.color = vec3f(0.0);
        out.offset = vec2f(0.0);
        out.shape = 0.0;
        return out;
    }
    // re_renderer sphere_or_circle_quad_span corner pattern.
    let local_idx = vertex_idx % 6u;
    let top_bottom = f32(local_idx <= 1u || local_idx == 5u) * 2.0 - 1.0;
    let left_right = f32(vertex_idx % 2u) * 2.0 - 1.0;

    let center = inst.pos_size.xyz;
    let depth = max(dot(center - frame.eye.xyz, frame.forward.xyz), 1e-3);
    let pixel_world = frame.params.x * depth;
    // re_renderer world_size_from_point_size as a floor; the ceiling keeps a close
    // zoom from turning sprites into full-screen quads. Same clamp as pick::core_px.
    let core = clamp(inst.pos_size.w, frame.params.y * pixel_world, frame.limits.x * pixel_world);
    // re_renderer circle_quad: half a pixel of margin for the feathered edge.
    let half = core * extent + 0.5 * pixel_world;
    let local = vec2f(left_right, top_bottom) * half;
    let world = center + frame.right.xyz * local.x + frame.up.xyz * local.y;

    out.position = frame.view_proj * vec4f(world, 1.0);
    out.color = inst.color.rgb;
    out.offset = local / core;
    out.shape = inst.color.a;
    return out;
}

@vertex
fn vs_core(@builtin(vertex_index) vertex_idx: u32, inst: Instance) -> VertexOut {
    return span(vertex_idx, inst, 1.0);
}

@vertex
fn vs_halo(@builtin(vertex_index) vertex_idx: u32, inst: Instance) -> VertexOut {
    return span(vertex_idx, inst, frame.params.z);
}

// The shape's radius-like distance, 1 on its outline: a circle for code, actors and
// proxy rings, a rounded card for work items (1 plus the rounded box's signed
// distance). Written for kGraf; the shapes follow the style guide's one shape per
// population (STG-DRAW-1): item dots, actor disks, task cards, proxy rings.
fn shape_distance(offset: vec2f, shape: f32) -> f32 {
    if shape > 2.5 && shape < 3.5 {
        let half = vec2f(1.0, CARD_HALF_HEIGHT) - vec2f(CARD_CORNER);
        let q = abs(offset) - half;
        return 1.0 + length(max(q, vec2f(0.0))) + min(max(q.x, q.y), 0.0) - CARD_CORNER;
    }
    return length(offset);
}

// re_renderer circle_quad_coverage, in core-radius units, written as
// 1 - smoothstep(lo, hi, r) with the feather width kept above zero (smoothstep with
// lo >= hi is undefined in WGSL).
fn core_coverage(r: f32) -> f32 {
    let feather = max(fwidth(r) * 0.5, 1e-4);
    return 1.0 - smoothstep(1.0 - feather, 1.0 + feather, r);
}

@fragment
fn fs_core(in: VertexOut) -> @location(0) vec4f {
    let r = shape_distance(in.offset, in.shape);
    let cov = core_coverage(r);
    // Only the inside writes depth; the feathered rim is left to blending. A proxy is
    // a ring (decision 0068): its middle is empty, so what is behind shows through.
    if cov < 0.5 || (in.shape > 3.5 && r < RING_INNER) {
        discard;
    }
    var color = in.color;
    if in.shape > 3.5 {
        // A proxy ring is flat.
    } else if in.shape > 2.5 {
        // A card: the palette's fill, and its state tint as the border.
        color = select(frame.card.rgb, in.color, r > 1.0 - CARD_BORDER);
    } else if in.shape > 1.5 {
        // An actor: its fill, with a lighter border.
        color = select(in.color, mix(in.color, vec3f(1.0), ACTOR_BORDER_MIX), r > ACTOR_BORDER);
    } else if frame.params.w > 0.5 {
        // re_renderer point_cloud.wgsl quick shading.
        color = in.color * max(0.4, sqrt(max(1.2 - r, 0.0)));
    }
    return vec4f(color * cov, cov);
}

@fragment
fn fs_halo(in: VertexOut) -> @location(0) vec4f {
    let r = shape_distance(in.offset, in.shape);
    let cov = core_coverage(r);
    let extent = frame.params.z;
    let t = clamp((r - 1.0) / max(extent - 1.0, 1e-3), 0.0, 1.0);
    // Outside the shape only, plus the rim the core pass discarded.
    let halo = HALO_STRENGTH * (1.0 - t) * (1.0 - t) * (1.0 - cov);
    if halo <= 0.0 {
        discard;
    }
    return vec4f(in.color * halo, halo);
}
