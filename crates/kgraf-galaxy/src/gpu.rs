//! The wgpu side: three pipelines (sprite cores, edges, sprite halos), their buffers,
//! and an offscreen color-plus-depth target.
//!
//! Scope against re_renderer (decision 0055; see third_party/rerun-re_renderer/UPSTREAM).
//! re_renderer's point and line renderers sit on its render context, view builder,
//! draw-phase manager, CPU-write/GPU-read belt, data-texture allocator, resource pools
//! and shader import resolver, plus rerun's logging, tracing and mutex crates: about
//! 32,000 lines for its whole crate. kGraf draws two primitives into one target,
//! so the copy is the shader techniques (points.wgsl) and the test-adapter preference
//! (`tests/headless.rs`), and the plumbing here is written.
//!
//! Draw order within one pass, and why:
//! 1. sprite cores: depth test and depth write, so a near node covers a far one
//!    whatever the instance order (without depth, far nodes drawn later covered near
//!    ones and popped while orbiting);
//! 2. edges: additive, depth tested against the cores, never writing depth, so a
//!    line passes behind a nearer node but lines never occlude each other;
//! 3. sprite halos (GPU budget only): premultiplied over, depth tested, not writing,
//!    so a glow never covers a nearer core.
//!
//! Renders into a texture rather than egui's own render pass: the texture keeps the last
//! frame, so when egui repaints for other reasons (typing in the orchestrator box) the
//! galaxy is not redrawn, and the headless test and the benchmark use the same path as
//! the window.

use bytemuck::{Pod, Zeroable};

use crate::budget::RenderBudget;
use crate::camera::CameraFrame;
use crate::error::GalaxyError;
use crate::pick::MIN_CORE_PX;
use crate::selection::{EdgeVertex, NodeInstance, srgb_to_linear};

/// The offscreen target format. sRGB so blending happens in linear space; egui samples
/// it as a native texture.
pub const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Depth format for the sprite cores. Depth32Float is supported by every backend,
/// software rasterizers included.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The default stage background, `--stage-bg` `#07080F` (style guide 1.8), sRGB. The
/// palette may replace it ([`GalaxyRenderer::set_stage`]).
pub const BACKGROUND_SRGB: [u8; 3] = kgraf_core::palette::STAGE.background;

/// A buffer this far above what it holds is reallocated smaller, so one huge scene
/// does not pin its memory after the graph shrinks.
const SHRINK_FACTOR: u64 = 4;
/// Buffers at or below this size are never shrunk; reallocating them saves nothing.
const SHRINK_FLOOR: u64 = 64 * 1024;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct FrameUniform {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    forward: [f32; 4],
    right: [f32; 4],
    up: [f32; 4],
    params: [f32; 4],
    limits: [f32; 4],
    /// rgb: a work card's fill (linear), from the palette.
    card: [f32; 4],
}

impl FrameUniform {
    fn new(frame: &CameraFrame, budget: RenderBudget, edge_density: f32, card: [f32; 3]) -> Self {
        let v4 = |v: glam::Vec3| [v.x, v.y, v.z, 0.0];
        Self {
            view_proj: frame.view_proj.to_cols_array_2d(),
            eye: v4(frame.eye),
            forward: v4(frame.forward),
            right: v4(frame.right),
            up: v4(frame.up),
            params: [
                frame.pixel_world_size_per_depth,
                MIN_CORE_PX,
                budget.halo_extent(),
                if budget.shaded() { 1.0 } else { 0.0 },
            ],
            limits: [budget.max_core_px(), edge_density, 0.0, 0.0],
            card: [card[0], card[1], card[2], 0.0],
        }
    }
}

/// A vertex buffer sized to its contents: grows with headroom, shrinks when far too
/// large, and refuses contents beyond the device's `max_buffer_size` with an error
/// instead of letting wgpu validation panic.
#[derive(Debug)]
pub struct GrowBuffer {
    label: &'static str,
    buffer: Option<wgpu::Buffer>,
    len: u64,
}

impl GrowBuffer {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            buffer: None,
            len: 0,
        }
    }

    /// Bytes allocated on the GPU.
    pub fn capacity(&self) -> u64 {
        self.buffer.as_ref().map_or(0, wgpu::Buffer::size)
    }

    pub fn write(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bytes: &[u8],
    ) -> Result<(), GalaxyError> {
        let need = bytes.len() as u64;
        let max = device.limits().max_buffer_size;
        if need > max {
            return Err(GalaxyError::SceneTooLarge {
                what: self.label,
                bytes: need,
                max,
            });
        }
        self.len = need;
        if need == 0 {
            return Ok(());
        }
        let cap = self.capacity();
        let too_small = cap < need;
        let too_large = cap > SHRINK_FLOOR && cap > need.saturating_mul(SHRINK_FACTOR);
        if too_small || too_large {
            // Headroom so a growing graph does not reallocate on every insert, clamped
            // to the device limit (still at least `need`, which fits).
            let align = wgpu::COPY_BUFFER_ALIGNMENT;
            let wanted = (need + need / 2).next_multiple_of(align);
            let size = wanted
                .min(max / align * align)
                .max(need.next_multiple_of(align));
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if let Some(b) = &self.buffer {
            queue.write_buffer(b, 0, bytes);
        }
        Ok(())
    }

    /// Writes `ranges` (byte offset, bytes) into the existing buffer and sets the drawn
    /// length to `total` bytes, without reallocating. Returns `false`, writing nothing,
    /// when `total` does not fit the current allocation; the caller then uploads the
    /// whole contents with [`Self::write`]. This is what makes a live change cost its
    /// own bytes rather than the whole scene's.
    pub fn write_ranges<'a>(
        &mut self,
        queue: &wgpu::Queue,
        total: u64,
        ranges: impl IntoIterator<Item = (u64, &'a [u8])>,
    ) -> bool {
        let Some(buffer) = &self.buffer else {
            return false;
        };
        if total > buffer.size() {
            return false;
        }
        for (offset, bytes) in ranges {
            if !bytes.is_empty() {
                queue.write_buffer(buffer, offset, bytes);
            }
        }
        self.len = total;
        true
    }

    fn slice(&self) -> Option<wgpu::BufferSlice<'_>> {
        match &self.buffer {
            Some(b) if self.len > 0 => Some(b.slice(..self.len)),
            _ => None,
        }
    }
}

/// An offscreen color target with its depth buffer.
#[derive(Debug)]
pub struct RenderTarget {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub depth_view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

impl RenderTarget {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Result<Self, GalaxyError> {
        let max = device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Err(GalaxyError::InvalidTarget { width, height, max });
        }
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("galaxy target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("galaxy depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        Ok(Self {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            depth_view: depth.create_view(&wgpu::TextureViewDescriptor::default()),
            texture,
            width,
            height,
        })
    }

    /// Copies the target back as tightly packed sRGB RGBA rows. Blocks until the GPU is
    /// done; for tests and screenshots, never the frame loop.
    pub fn read_pixels(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Vec<u8>, GalaxyError> {
        let row = self.width * 4;
        let padded = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("galaxy readback"),
            size: u64::from(padded) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("galaxy readback"),
        });
        encoder.copy_texture_to_buffer(
            self.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            // A dropped receiver means the caller already gave up; nothing to report.
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GalaxyError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|e| GalaxyError::Readback(e.to_string()))?
            .map_err(|e| GalaxyError::Readback(e.to_string()))?;
        let data = slice
            .get_mapped_range()
            .map_err(|e| GalaxyError::Readback(e.to_string()))?;
        let mut out = Vec::with_capacity((row * self.height) as usize);
        for r in 0..self.height {
            let start = (r * padded) as usize;
            out.extend_from_slice(&data[start..start + row as usize]);
        }
        drop(data);
        buffer.unmap();
        Ok(out)
    }
}

/// Element count for a draw call, or an error when it exceeds `u32`.
fn draw_count(what: &'static str, len: usize) -> Result<u32, GalaxyError> {
    u32::try_from(len).map_err(|_| GalaxyError::SceneTooLarge {
        what,
        bytes: len as u64,
        max: u64::from(u32::MAX),
    })
}

/// Sprite and edge pipelines with the scene's GPU buffers.
#[derive(Debug)]
pub struct GalaxyRenderer {
    cores: wgpu::RenderPipeline,
    halos: wgpu::RenderPipeline,
    lines: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instances: GrowBuffer,
    instance_count: u32,
    edges: GrowBuffer,
    edge_vertex_count: u32,
    /// The clear color and a card's fill, sRGB 0..=1 (the palette's stage colors).
    background: [f32; 3],
    card: [f32; 3],
}

impl GalaxyRenderer {
    /// Sets the stage background and a work card's fill (sRGB 0..=1).
    pub fn set_stage(&mut self, background: [f32; 3], card: [f32; 3]) {
        self.background = background;
        self.card = card;
    }

    /// Pipelines for a `format` color target with a [`DEPTH_FORMAT`] depth target,
    /// single-sampled.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("galaxy frame"),
            size: std::mem::size_of::<FrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("galaxy frame"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("galaxy frame"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("galaxy"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let points_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("galaxy points"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/points.wgsl").into()),
        });
        let lines_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("galaxy lines"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/lines.wgsl").into()),
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<NodeInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
        };
        let edge_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<EdgeVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32],
        };

        struct Spec<'a> {
            label: &'a str,
            module: &'a wgpu::ShaderModule,
            vs: &'a str,
            fs: &'a str,
            buffers: &'a [Option<wgpu::VertexBufferLayout<'a>>],
            topology: wgpu::PrimitiveTopology,
            blend: wgpu::BlendState,
            depth_write: bool,
            depth_compare: wgpu::CompareFunction,
        }
        let pipeline = |s: Spec<'_>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(s.label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: s.module,
                    entry_point: Some(s.vs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: s.buffers,
                },
                primitive: wgpu::PrimitiveState {
                    topology: s.topology,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(s.depth_write),
                    depth_compare: Some(s.depth_compare),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: s.module,
                    entry_point: Some(s.fs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(s.blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };

        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let instances = [Some(instance_layout)];
        let cores = pipeline(Spec {
            label: "galaxy sprite cores",
            module: &points_module,
            vs: "vs_core",
            fs: "fs_core",
            buffers: &instances,
            topology: wgpu::PrimitiveTopology::TriangleList,
            blend: wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            depth_write: true,
            depth_compare: wgpu::CompareFunction::Less,
        });
        let halos = pipeline(Spec {
            label: "galaxy sprite halos",
            module: &points_module,
            vs: "vs_halo",
            fs: "fs_halo",
            buffers: &instances,
            topology: wgpu::PrimitiveTopology::TriangleList,
            blend: wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            depth_write: false,
            depth_compare: wgpu::CompareFunction::Less,
        });
        let lines = pipeline(Spec {
            label: "galaxy edges",
            module: &lines_module,
            vs: "vs_main",
            fs: "fs_main",
            buffers: &[Some(edge_layout)],
            topology: wgpu::PrimitiveTopology::LineList,
            blend: additive,
            depth_write: false,
            depth_compare: wgpu::CompareFunction::LessEqual,
        });

        Self {
            cores,
            halos,
            lines,
            uniform,
            bind_group,
            instances: GrowBuffer::new("galaxy instances"),
            instance_count: 0,
            edges: GrowBuffer::new("galaxy edges"),
            edge_vertex_count: 0,
            background: BACKGROUND_SRGB.map(|c| f32::from(c) / 255.0),
            card: crate::palette::Palette::default().card,
        }
    }

    pub fn set_instances(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[NodeInstance],
    ) -> Result<(), GalaxyError> {
        let count = draw_count("galaxy instances", instances.len())?;
        self.instances
            .write(device, queue, bytemuck::cast_slice(instances))?;
        self.instance_count = count;
        Ok(())
    }

    pub fn set_edges(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        edges: &[EdgeVertex],
    ) -> Result<(), GalaxyError> {
        let count = draw_count("galaxy edges", edges.len())?;
        self.edges
            .write(device, queue, bytemuck::cast_slice(edges))?;
        self.edge_vertex_count = count;
        Ok(())
    }

    /// Rewrites the given runs of instances in place (`(first instance, run)`), with
    /// `count` instances drawn afterwards. `Ok(false)` when `count` no longer fits the
    /// allocation; nothing was written and the caller uploads everything.
    pub fn patch_instances(
        &mut self,
        queue: &wgpu::Queue,
        count: usize,
        runs: &[(u32, &[NodeInstance])],
    ) -> Result<bool, GalaxyError> {
        let count32 = draw_count("galaxy instances", count)?;
        let stride = std::mem::size_of::<NodeInstance>() as u64;
        let ranges = runs
            .iter()
            .map(|(first, run)| (u64::from(*first) * stride, bytemuck::cast_slice(run)));
        let ok = self
            .instances
            .write_ranges(queue, count as u64 * stride, ranges);
        if ok {
            self.instance_count = count32;
        }
        Ok(ok)
    }

    /// [`Self::patch_instances`] for edge vertices.
    pub fn patch_edges(
        &mut self,
        queue: &wgpu::Queue,
        count: usize,
        runs: &[(u32, &[EdgeVertex])],
    ) -> Result<bool, GalaxyError> {
        let count32 = draw_count("galaxy edges", count)?;
        let stride = std::mem::size_of::<EdgeVertex>() as u64;
        let ranges = runs
            .iter()
            .map(|(first, run)| (u64::from(*first) * stride, bytemuck::cast_slice(run)));
        let ok = self
            .edges
            .write_ranges(queue, count as u64 * stride, ranges);
        if ok {
            self.edge_vertex_count = count32;
        }
        Ok(ok)
    }

    pub fn instance_count(&self) -> u32 {
        self.instance_count
    }

    pub fn edge_vertex_count(&self) -> u32 {
        self.edge_vertex_count
    }

    /// GPU bytes held for instances and edges.
    pub fn buffer_capacity(&self) -> (u64, u64) {
        (self.instances.capacity(), self.edges.capacity())
    }

    /// Clears `target` to the background and draws cores, edges, then halos (see the
    /// module comment). Writes the frame uniform through `queue`, so call once per
    /// submit. `edge_density` multiplies the edges whose vertices defer density
    /// ([`EdgeVertex::density`]). `timestamps` brackets the pass for GPU timing where
    /// the device has `TIMESTAMP_QUERY`.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &RenderTarget,
        frame: &CameraFrame,
        budget: RenderBudget,
        edge_density: f32,
        timestamps: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        queue.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&FrameUniform::new(
                frame,
                budget,
                edge_density,
                self.card.map(srgb_to_linear),
            )),
        );
        let [r, g, b] = self.background.map(|c| f64::from(srgb_to_linear(c)));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("galaxy"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &target.depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: timestamps,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &self.bind_group, &[]);
        let sprites = self.instances.slice();
        if let Some(slice) = sprites {
            pass.set_pipeline(&self.cores);
            pass.set_vertex_buffer(0, slice);
            pass.draw(0..6, 0..self.instance_count);
        }
        if let Some(slice) = self.edges.slice() {
            pass.set_pipeline(&self.lines);
            pass.set_vertex_buffer(0, slice);
            pass.draw(0..self.edge_vertex_count, 0..1);
        }
        if let Some(slice) = sprites
            && budget.halos()
        {
            pass.set_pipeline(&self.halos);
            pass.set_vertex_buffer(0, slice);
            pass.draw(0..6, 0..self.instance_count);
        }
    }
}
