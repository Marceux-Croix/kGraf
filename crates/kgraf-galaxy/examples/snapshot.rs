//! Renders the demo galaxy offscreen and writes a binary PPM, to inspect the look
//! without the shell: `cargo run --release -p kgraf-galaxy --example snapshot --
//! <nodes> <gpu|cpu> <out.ppm> [selected-node]`.

use std::io::Write;

use kgraf_galaxy::gpu::RenderTarget;
use kgraf_galaxy::{Galaxy, RenderBudget, Scene, demo};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        return Err("usage: snapshot <nodes> <gpu|cpu> <out.ppm> [selected-node]".into());
    }
    let n: usize = args[1].parse()?;
    let budget = match args[2].as_str() {
        "cpu" => RenderBudget::Cpu,
        "gpu" => RenderBudget::Gpu,
        other => return Err(format!("budget must be gpu or cpu, got {other}").into()),
    };
    let selected: Option<u32> = args.get(4).map(|s| s.parse()).transpose()?;

    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))?;
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;
    eprintln!("adapter: {}", adapter.get_info().name);

    let mut galaxy = Galaxy::new(&device, Scene::new(demo::synthetic(n, 7))?, budget);
    if selected.is_some() {
        galaxy.select(selected);
        while galaxy.update(1.0 / 60.0) {}
    }
    let (w, h) = (1200u32, 800u32);
    let target = RenderTarget::new(&device, w, h)?;
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    galaxy.render(&device, &queue, &mut encoder, &target)?;
    queue.submit([encoder.finish()]);
    let pixels = target.read_pixels(&device, &queue)?;

    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[3])?);
    writeln!(out, "P6 {w} {h} 255")?;
    for px in pixels.chunks_exact(4) {
        out.write_all(&px[..3])?;
    }
    Ok(())
}
