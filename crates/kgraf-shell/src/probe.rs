//! Headless adapter probe: what this machine offers, without opening a window.
//!
//! Used by `kgraf-app --probe` to answer the 0054 question "is there a software
//! path here?" with what `wgpu` actually returns, including the result of
//! `force_fallback_adapter`, rather than with what the documentation implies.

use std::fmt::Write as _;

use crate::adapter::{AdapterFacts, AdapterPolicy, RenderPath, classify, select_adapter};

#[derive(Debug)]
pub struct ProbeReport {
    /// Every adapter across every backend compiled into this build.
    pub adapters: Vec<(AdapterFacts, RenderPath)>,
    /// `request_adapter` with `force_fallback_adapter: true`: the adapter, or the error
    /// text `wgpu` returned.
    pub forced_fallback: Result<(AdapterFacts, RenderPath), String>,
    /// What the shell's own selector would pick with the software-only policy.
    pub software_only_selection: Result<String, String>,
}

impl ProbeReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "adapters enumerated (all backends): {}",
            self.adapters.len()
        );
        for (facts, path) in &self.adapters {
            let _ = writeln!(out, "  - {} -> {:?}", facts.summary(), path);
        }
        match &self.forced_fallback {
            Ok((facts, path)) => {
                let _ = writeln!(
                    out,
                    "force_fallback_adapter: {} -> {:?}",
                    facts.summary(),
                    path
                );
            }
            Err(e) => {
                let _ = writeln!(out, "force_fallback_adapter: no adapter ({e})");
            }
        }
        match &self.software_only_selection {
            Ok(s) => {
                let _ = writeln!(out, "software-only selection: {s}");
            }
            Err(e) => {
                let _ = writeln!(out, "software-only selection: error: {e}");
            }
        }
        out
    }
}

pub fn run() -> ProbeReport {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
    });
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    let facts: Vec<AdapterFacts> = adapters
        .iter()
        .map(|a| AdapterFacts::from_info(&a.get_info(), true))
        .collect();

    let forced_fallback =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: true,
            ..Default::default()
        }))
        .map(|a| {
            let f = AdapterFacts::from_info(&a.get_info(), true);
            let p = classify(&f);
            (f, p)
        })
        .map_err(|e| e.to_string());

    let software_only_selection = select_adapter(&facts, AdapterPolicy::SoftwareOnly)
        .map(|s| facts[s.index].summary())
        .map_err(|e| e.to_string());

    ProbeReport {
        adapters: facts
            .into_iter()
            .map(|f| {
                let p = classify(&f);
                (f, p)
            })
            .collect(),
        forced_fallback,
        software_only_selection,
    }
}
