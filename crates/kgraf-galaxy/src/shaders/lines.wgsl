// Edges: one line-list segment per edge, per-vertex color already scaled by intensity,
// blended additively, depth tested against the sprite cores but never writing depth
// (the reference's "Nodes and edges" rendering).
// Written for kGraf: re_renderer's lines.wgsl draws thick quads with caps and joins,
// which is far more fill than the reference's one-pixel additive segments need.

struct Frame {
    view_proj: mat4x4f,
    eye: vec4f,
    forward: vec4f,
    right: vec4f,
    up: vec4f,
    params: vec4f,
    limits: vec4f,
    // rgb: a work card's fill (linear), from the palette.
    card: vec4f,
};

@group(0) @binding(0)
var<uniform> frame: Frame;

struct VertexIn {
    @location(0) position: vec3f,
    @location(1) color: vec3f,
    // 1: multiply by the frame's density scale (frame.limits.y); 0: color is final.
    // Density lives in the uniform so an edge-count change rewrites no vertices.
    @location(2) density: f32,
};

struct VertexOut {
    @builtin(position) position: vec4f,
    @location(0) color: vec3f,
};

@vertex
fn vs_main(v: VertexIn) -> VertexOut {
    var out: VertexOut;
    out.position = frame.view_proj * vec4f(v.position, 1.0);
    out.color = v.color * mix(1.0, frame.limits.y, v.density);
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4f {
    // Alpha 0: additive color only, the target's alpha stays as cleared (opaque).
    return vec4f(in.color, 0.0);
}
