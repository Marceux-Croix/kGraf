//! Frame-time benchmarks (measured budgets) into a 1920x1080 target.
//!
//! Two measures per case, because they answer different questions:
//! - **wall** (criterion): encode, submit and wait for completion (`device.poll`). It
//!   includes a fixed submit-and-wait cost, so the empty scene is benched as the
//!   baseline to subtract.
//! - **gpu** (printed after criterion): the pass's own GPU time from timestamp queries,
//!   when the adapter has `TIMESTAMP_QUERY`. Every sample is checked: a sample whose end
//!   is not after its start is invalid, and a case with invalid samples is reported as
//!   such, never as a time. Measured on an Apple M4 Max (Metal): pass-boundary
//!   timestamps come back with end before start whenever the pass draws anything, so
//!   there the honest GPU estimate is wall time minus the empty-scene baseline.
//!
//! Views: `home` (the start view, whole galaxy on screen), `orbit` (the camera turns
//! every frame, as while dragging), `inside` (flown into a cluster, where fill is
//! highest). Also the event-driven costs: a selection change and a click pick.
//!
//! Run: `cargo bench -p kgraf-galaxy --bench frame`.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec3;
use kgraf_galaxy::gpu::RenderTarget;
use kgraf_galaxy::{Galaxy, RenderBudget, Scene, SceneInput, demo};

const W: u32 = 1920;
const H: u32 = 1080;
const GPU_SAMPLES: usize = 200;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    timestamps: bool,
    /// Timestamps written on the encoder around the pass, not at pass boundaries.
    encoder_timestamps: bool,
}

fn gpu() -> Gpu {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .expect("adapter");
    let info = adapter.get_info();
    let features = adapter.features();
    let timestamps = features.contains(wgpu::Features::TIMESTAMP_QUERY);
    let encoder_timestamps =
        timestamps && features.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
    eprintln!(
        "adapter: {} ({:?}, {}), TIMESTAMP_QUERY: {timestamps}, INSIDE_ENCODERS: {encoder_timestamps}",
        info.name, info.device_type, info.backend
    );
    let mut required_features = wgpu::Features::empty();
    if timestamps {
        required_features |= wgpu::Features::TIMESTAMP_QUERY;
    }
    if encoder_timestamps {
        required_features |= wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
    }
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_features,
        ..Default::default()
    }))
    .expect("device");
    Gpu {
        device,
        queue,
        timestamps,
        encoder_timestamps,
    }
}

#[derive(Clone, Copy)]
enum View {
    Home,
    Orbit,
    Inside,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Orbit => "orbit",
            Self::Inside => "inside",
        }
    }

    fn setup(self, g: &mut Galaxy) {
        match self {
            Self::Home => {}
            Self::Orbit => g.camera_mut().rotate(0.0, 150.0, H as f32),
            Self::Inside => {
                let Some(p) = g.scene().nodes().first().map(|n| Vec3::from(n.position)) else {
                    return;
                };
                g.camera_mut().fly_to(p, 60.0);
            }
        }
        while g.update(1.0 / 60.0) {}
    }

    /// Per-frame camera change: the orbit view turns 2 px of drag per frame.
    fn step(self, g: &mut Galaxy) {
        if let Self::Orbit = self {
            g.camera_mut().rotate(2.0, 0.0, H as f32);
            g.update(1.0 / 60.0);
        }
    }
}

fn frame(g: &mut Galaxy, gpu: &Gpu, target: &RenderTarget) {
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    g.render(&gpu.device, &gpu.queue, &mut encoder, target)
        .expect("render");
    gpu.queue.submit([encoder.finish()]);
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
}

/// GPU timing result for one case.
enum GpuTime {
    Unsupported,
    /// Samples whose end was not after their start, out of the total.
    Invalid {
        bad: usize,
        total: usize,
    },
    Valid {
        p50_ms: f64,
        p95_ms: f64,
    },
}

/// Median and p95 GPU pass time in milliseconds over `GPU_SAMPLES` frames.
fn gpu_times(g: &mut Galaxy, view: View, gpu: &Gpu, target: &RenderTarget) -> GpuTime {
    if !gpu.timestamps {
        return GpuTime::Unsupported;
    }
    let set = gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
        label: Some("galaxy bench"),
        ty: wgpu::QueryType::Timestamp,
        count: 2,
    });
    let resolve = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("timestamps resolve"),
        size: 16,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let read = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("timestamps read"),
        size: 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let period_ns = f64::from(gpu.queue.get_timestamp_period());
    let mut ms = Vec::with_capacity(GPU_SAMPLES);
    let mut bad = 0;
    for _ in 0..GPU_SAMPLES {
        view.step(g);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        if gpu.encoder_timestamps {
            encoder.write_timestamp(&set, 0);
            g.render(&gpu.device, &gpu.queue, &mut encoder, target)
                .expect("render");
            encoder.write_timestamp(&set, 1);
        } else {
            g.render_timed(
                &gpu.device,
                &gpu.queue,
                &mut encoder,
                target,
                Some(wgpu::RenderPassTimestampWrites {
                    query_set: &set,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
            )
            .expect("render");
        }
        encoder.resolve_query_set(&set, 0..2, &resolve, 0);
        encoder.copy_buffer_to_buffer(&resolve, 0, &read, 0, 16);
        gpu.queue.submit([encoder.finish()]);
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let ticks: [u64; 2] = {
            let data = read.slice(..).get_mapped_range().expect("mapped");
            bytemuck::pod_read_unaligned(&data[..16])
        };
        read.unmap();
        if ticks[0] == 0 || ticks[1] <= ticks[0] {
            bad += 1;
            continue;
        }
        ms.push((ticks[1] - ticks[0]) as f64 * period_ns / 1e6);
    }
    if bad > 0 {
        return GpuTime::Invalid {
            bad,
            total: GPU_SAMPLES,
        };
    }
    ms.sort_by(f64::total_cmp);
    GpuTime::Valid {
        p50_ms: ms[ms.len() / 2],
        p95_ms: ms[ms.len() * 95 / 100],
    }
}

fn benches(c: &mut Criterion) {
    let gpu = gpu();
    let target = RenderTarget::new(&gpu.device, W, H).expect("target");
    let mut gpu_report = Vec::new();
    let scenes = [
        (0usize, Scene::new(SceneInput::default()).expect("empty")),
        (
            10_000,
            Scene::new(demo::synthetic(10_000, 7)).expect("scene"),
        ),
        (
            100_000,
            Scene::new(demo::synthetic(100_000, 7)).expect("scene"),
        ),
    ];
    for (n, scene) in &scenes {
        let n = *n;
        eprintln!("{n} nodes, {} edges", scene.edges().len());
        for (budget_name, budget) in [("gpu", RenderBudget::Gpu), ("cpu", RenderBudget::Cpu)] {
            for view in [View::Home, View::Orbit, View::Inside] {
                if n == 0 && !matches!(view, View::Home) {
                    continue;
                }
                let mut g = Galaxy::new(&gpu.device, scene.clone(), budget);
                view.setup(&mut g);
                frame(&mut g, &gpu, &target); // upload outside the timed loop
                let id = format!("frame_{budget_name}_budget/{}", view.name());
                c.bench_with_input(BenchmarkId::new(id.clone(), n), &n, |b, _| {
                    b.iter(|| {
                        view.step(&mut g);
                        frame(&mut g, &gpu, &target);
                    });
                });
                gpu_report.push((format!("{id}/{n}"), gpu_times(&mut g, view, &gpu, &target)));
            }
        }
        if n == 0 {
            continue;
        }

        let mut g = Galaxy::new(&gpu.device, scene.clone(), RenderBudget::Gpu);
        frame(&mut g, &gpu, &target);
        let mut k = 0u32;
        c.bench_with_input(BenchmarkId::new("select_and_frame", n), &n, |b, _| {
            b.iter(|| {
                k = (k + 7919) % n as u32;
                g.select(Some(k));
                frame(&mut g, &gpu, &target);
            });
        });

        // A search result (0032): 500 matches lit at once, then the frame, on both
        // budgets. The set changes every iteration so each one rebuilds.
        for (budget_name, budget) in [("gpu", RenderBudget::Gpu), ("cpu", RenderBudget::Cpu)] {
            let mut g = Galaxy::new(&gpu.device, scene.clone(), budget);
            frame(&mut g, &gpu, &target);
            let mut k = 0u32;
            c.bench_with_input(
                BenchmarkId::new(format!("select_set_500_and_frame_{budget_name}"), n),
                &n,
                |b, _| {
                    b.iter(|| {
                        k = (k + 7919) % n as u32;
                        let set: Vec<u32> = (0..500u32).map(|i| (k + i * 13) % n as u32).collect();
                        g.select_set(&set);
                        frame(&mut g, &gpu, &target);
                    });
                },
            );
        }

        let g = Galaxy::new(&gpu.device, scene.clone(), RenderBudget::Gpu);
        let center = [W as f32 * 0.5, H as f32 * 0.5];
        c.bench_with_input(BenchmarkId::new("pick", n), &n, |b, _| {
            b.iter(|| std::hint::black_box(g.pick(center, W, H)));
        });
    }
    let how = if gpu.encoder_timestamps {
        "encoder"
    } else {
        "pass-boundary"
    };
    eprintln!("GPU time from {how} timestamp queries ({GPU_SAMPLES} frames each):");
    for (id, t) in gpu_report {
        match t {
            GpuTime::Valid { p50_ms, p95_ms } => {
                eprintln!("  gpu {id}: p50 {p50_ms:.3} ms, p95 {p95_ms:.3} ms");
            }
            GpuTime::Invalid { bad, total } => eprintln!(
                "  gpu {id}: INVALID, {bad} of {total} samples had end <= start; use wall minus the empty baseline"
            ),
            GpuTime::Unsupported => eprintln!("  gpu {id}: unavailable (no TIMESTAMP_QUERY)"),
        }
    }
}

criterion_group! {
    name = frame_benches;
    config = Criterion::default().sample_size(30);
    targets = benches
}
criterion_main!(frame_benches);
