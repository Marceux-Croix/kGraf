//! GPU adapter classification and selection (decision 0054).
//!
//! The shell must know when it renders on a CPU and say so. Everything here is a pure
//! function over [`AdapterFacts`], a plain copy of the fields of `wgpu::AdapterInfo`
//! that the decision needs, so the policy is unit-tested without a GPU.

use crate::error::ShellError;

/// The device type as `wgpu` reports it, mirrored so the policy does not depend on the
/// `wgpu` major version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    DiscreteGpu,
    IntegratedGpu,
    VirtualGpu,
    Cpu,
    Other,
}

impl From<wgpu::DeviceType> for DeviceKind {
    fn from(t: wgpu::DeviceType) -> Self {
        match t {
            wgpu::DeviceType::DiscreteGpu => Self::DiscreteGpu,
            wgpu::DeviceType::IntegratedGpu => Self::IntegratedGpu,
            wgpu::DeviceType::VirtualGpu => Self::VirtualGpu,
            wgpu::DeviceType::Cpu => Self::Cpu,
            wgpu::DeviceType::Other => Self::Other,
        }
    }
}

/// The adapter fields the render-path decision reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterFacts {
    pub name: String,
    pub driver: String,
    pub driver_info: String,
    pub backend: String,
    pub device_kind: DeviceKind,
    /// Whether the adapter can present to the window surface. `true` when no surface
    /// was available to check against (headless probe).
    pub surface_compatible: bool,
}

impl AdapterFacts {
    pub fn from_info(info: &wgpu::AdapterInfo, surface_compatible: bool) -> Self {
        Self {
            name: info.name.clone(),
            driver: info.driver.clone(),
            driver_info: info.driver_info.clone(),
            backend: info.backend.to_string(),
            device_kind: info.device_type.into(),
            surface_compatible,
        }
    }

    /// One-line description for logs and the status indicator.
    pub fn summary(&self) -> String {
        format!(
            "{} ({:?}, {} backend, driver {:?})",
            self.name, self.device_kind, self.backend, self.driver
        )
    }
}

/// Why an adapter counts as software.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftwareReason {
    /// The driver reports `DeviceType::Cpu`.
    CpuDeviceType,
    /// The name or driver matches a known software rasterizer, whatever the device type
    /// says. Some drivers report llvmpipe as `Other`, so the type alone is not enough.
    KnownRasterizer(&'static str),
}

/// Where frames are rasterized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderPath {
    Hardware,
    Software(SoftwareReason),
}

impl RenderPath {
    pub fn is_software(self) -> bool {
        matches!(self, Self::Software(_))
    }
}

/// Known software rasterizers, matched case-insensitively against name, driver and
/// driver info. The label is what the indicator shows.
///
/// `warp` is matched as a whole word so that an unrelated name containing the letters
/// (for example "Warpspeed") is not misread; WARP on DX12 usually reports its adapter
/// name as "Microsoft Basic Render Driver", which is matched separately.
const RASTERIZERS: &[(&str, &str)] = &[
    ("llvmpipe", "llvmpipe"),
    ("lavapipe", "lavapipe"),
    ("softpipe", "softpipe"),
    ("swiftshader", "SwiftShader"),
    ("microsoft basic render driver", "WARP"),
    ("apple software renderer", "Apple Software Renderer"),
];

fn contains_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token == word)
}

/// Classifies one adapter. Pure.
pub fn classify(facts: &AdapterFacts) -> RenderPath {
    let text = format!("{} {} {}", facts.name, facts.driver, facts.driver_info).to_lowercase();
    for (needle, label) in RASTERIZERS {
        if text.contains(needle) {
            return RenderPath::Software(SoftwareReason::KnownRasterizer(label));
        }
    }
    if contains_word(&text, "warp") {
        return RenderPath::Software(SoftwareReason::KnownRasterizer("WARP"));
    }
    if facts.device_kind == DeviceKind::Cpu {
        return RenderPath::Software(SoftwareReason::CpuDeviceType);
    }
    RenderPath::Hardware
}

/// Which hardware adapter to prefer when several can present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpuPreference {
    /// Integrated first. The default: a desktop shell that idles most of the time
    /// should not wake a discrete GPU (and drain a laptop battery) to draw panels.
    #[default]
    LowPower,
    /// Discrete first. The "Prefer high-performance GPU" setting.
    HighPerformance,
}

impl GpuPreference {
    pub fn from_setting(prefer_high_performance: bool) -> Self {
        if prefer_high_performance {
            Self::HighPerformance
        } else {
            Self::LowPower
        }
    }

    pub fn wgpu(self) -> wgpu::PowerPreference {
        match self {
            Self::LowPower => wgpu::PowerPreference::LowPower,
            Self::HighPerformance => wgpu::PowerPreference::HighPerformance,
        }
    }
}

/// Which adapters the selector may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterPolicy {
    /// Hardware first, ordered by the preference; a software adapter only when no
    /// hardware adapter can present, and then the indicator says so.
    PreferHardware(GpuPreference),
    /// Software adapters only. Used to exercise the CPU path on a machine with a GPU.
    SoftwareOnly,
}

impl Default for AdapterPolicy {
    fn default() -> Self {
        Self::PreferHardware(GpuPreference::default())
    }
}

/// Preference order among hardware adapters: lower is better.
fn hardware_rank(kind: DeviceKind, preference: GpuPreference) -> u8 {
    match (kind, preference) {
        (DeviceKind::IntegratedGpu, GpuPreference::LowPower) => 0,
        (DeviceKind::DiscreteGpu, GpuPreference::LowPower) => 1,
        (DeviceKind::DiscreteGpu, GpuPreference::HighPerformance) => 0,
        (DeviceKind::IntegratedGpu, GpuPreference::HighPerformance) => 1,
        (DeviceKind::VirtualGpu, _) => 2,
        (DeviceKind::Other, _) => 3,
        // A Cpu device is classified as software before ranking is consulted.
        (DeviceKind::Cpu, _) => 4,
    }
}

/// The selector's decision: the index into the input slice and the path it implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub index: usize,
    pub path: RenderPath,
}

/// What to do about a missing adapter on this platform. Measured on macOS 27 with
/// wgpu 30: Metal reports "no fallback adapters", so there is no CPU path there.
const NO_ADAPTER_HINT: &str = if cfg!(target_os = "macos") {
    "On macOS, Metal exposes no software adapter; the CPU path exists on Linux \
     (llvmpipe/lavapipe) and Windows (WARP)"
} else if cfg!(target_os = "windows") {
    "Windows normally provides WARP; check that D3D12 is available"
} else {
    "Install Mesa's llvmpipe (GL) or lavapipe (Vulkan) software driver"
};

/// Picks an adapter under `policy`. Pure.
///
/// Returns an error, never a silent default, when nothing suitable exists. The
/// error lists every adapter seen so the user can tell "no adapter at all" from "an
/// adapter that cannot present to this window".
pub fn select_adapter(
    adapters: &[AdapterFacts],
    policy: AdapterPolicy,
) -> Result<Selection, ShellError> {
    let candidates = adapters
        .iter()
        .enumerate()
        .filter(|(_, a)| a.surface_compatible)
        .map(|(i, a)| (i, classify(a), a));

    let first_software = candidates.clone().find(|(_, path, _)| path.is_software());

    let pick = match policy {
        AdapterPolicy::PreferHardware(preference) => candidates
            .clone()
            .filter(|(_, path, _)| *path == RenderPath::Hardware)
            .min_by_key(|(i, _, a)| (hardware_rank(a.device_kind, preference), *i))
            .or(first_software),
        AdapterPolicy::SoftwareOnly => first_software,
    };

    match pick {
        Some((index, path, _)) => Ok(Selection { index, path }),
        None => Err(ShellError::NoAdapter {
            policy,
            hint: NO_ADAPTER_HINT,
            seen: if adapters.is_empty() {
                "none".to_owned()
            } else {
                adapters
                    .iter()
                    .map(|a| {
                        format!(
                            "[{}; path {:?}; presents to window: {}]",
                            a.summary(),
                            classify(a),
                            a.surface_compatible
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: &str, kind: DeviceKind) -> AdapterFacts {
        AdapterFacts {
            name: name.to_owned(),
            driver: String::new(),
            driver_info: String::new(),
            backend: "vulkan".to_owned(),
            device_kind: kind,
            surface_compatible: true,
        }
    }

    #[test]
    fn apple_gpu_is_hardware() {
        let a = AdapterFacts {
            backend: "metal".to_owned(),
            ..facts("Apple M2 Pro", DeviceKind::IntegratedGpu)
        };
        assert_eq!(classify(&a), RenderPath::Hardware);
    }

    #[test]
    fn cpu_device_type_is_software_whatever_the_name() {
        assert_eq!(
            classify(&facts("Some Vendor Device", DeviceKind::Cpu)),
            RenderPath::Software(SoftwareReason::CpuDeviceType)
        );
    }

    #[test]
    fn llvmpipe_is_software_even_when_typed_other() {
        // Mesa llvmpipe over GL reports DeviceType::Other on some drivers.
        let a = facts("llvmpipe (LLVM 17.0.6, 256 bits)", DeviceKind::Other);
        assert_eq!(
            classify(&a),
            RenderPath::Software(SoftwareReason::KnownRasterizer("llvmpipe"))
        );
    }

    #[test]
    fn lavapipe_detected_from_driver_field() {
        let a = AdapterFacts {
            driver: "llvmpipe".to_owned(),
            driver_info: "Mesa 24.0.5 (LLVM 17.0.6)".to_owned(),
            ..facts("lavapipe", DeviceKind::Cpu)
        };
        assert!(classify(&a).is_software());
    }

    #[test]
    fn warp_detected_by_adapter_name_and_by_word() {
        let by_name = AdapterFacts {
            backend: "dx12".to_owned(),
            ..facts("Microsoft Basic Render Driver", DeviceKind::Other)
        };
        assert_eq!(
            classify(&by_name),
            RenderPath::Software(SoftwareReason::KnownRasterizer("WARP"))
        );
        let by_word = facts("WARP adapter", DeviceKind::Other);
        assert!(classify(&by_word).is_software());
    }

    #[test]
    fn warp_substring_inside_a_word_is_not_warp() {
        assert_eq!(
            classify(&facts("Warpspeed GPU 9000", DeviceKind::DiscreteGpu)),
            RenderPath::Hardware
        );
    }

    #[test]
    fn swiftshader_detected_case_insensitively() {
        let a = facts("SwiftShader Device (Subzero)", DeviceKind::Cpu);
        assert_eq!(
            classify(&a),
            RenderPath::Software(SoftwareReason::KnownRasterizer("SwiftShader"))
        );
    }

    const LOW: AdapterPolicy = AdapterPolicy::PreferHardware(GpuPreference::LowPower);
    const HIGH: AdapterPolicy = AdapterPolicy::PreferHardware(GpuPreference::HighPerformance);

    fn mixed() -> Vec<AdapterFacts> {
        vec![
            facts("llvmpipe", DeviceKind::Cpu),
            facts("NVIDIA RTX", DeviceKind::DiscreteGpu),
            facts("Intel UHD", DeviceKind::IntegratedGpu),
        ]
    }

    #[test]
    fn default_policy_is_low_power() {
        assert_eq!(AdapterPolicy::default(), LOW);
        assert_eq!(GpuPreference::from_setting(false), GpuPreference::LowPower);
    }

    #[test]
    fn low_power_prefers_integrated_then_discrete_then_software() {
        let list = mixed();
        let s = select_adapter(&list, LOW).expect("selects");
        assert_eq!(list[s.index].name, "Intel UHD");
        assert_eq!(s.path, RenderPath::Hardware);

        let no_integrated = vec![list[0].clone(), list[1].clone()];
        let s = select_adapter(&no_integrated, LOW).expect("selects");
        assert_eq!(no_integrated[s.index].name, "NVIDIA RTX");
    }

    #[test]
    fn high_performance_prefers_discrete_then_integrated() {
        let list = mixed();
        let s = select_adapter(&list, HIGH).expect("selects");
        assert_eq!(list[s.index].name, "NVIDIA RTX");

        let no_discrete = vec![list[0].clone(), list[2].clone()];
        let s = select_adapter(&no_discrete, HIGH).expect("selects");
        assert_eq!(no_discrete[s.index].name, "Intel UHD");
    }

    #[test]
    fn software_is_last_under_either_preference() {
        for policy in [LOW, HIGH] {
            let list = vec![
                facts("lavapipe", DeviceKind::Cpu),
                facts("Virtio GPU", DeviceKind::VirtualGpu),
            ];
            let s = select_adapter(&list, policy).expect("selects");
            assert_eq!(s.index, 1, "{policy:?}");
            assert_eq!(s.path, RenderPath::Hardware);
        }
    }

    #[test]
    fn falls_back_to_software_when_no_hardware() {
        let list = vec![facts("llvmpipe", DeviceKind::Cpu)];
        let s = select_adapter(&list, LOW).expect("selects");
        assert_eq!(s.index, 0);
        assert!(s.path.is_software());
    }

    #[test]
    fn hardware_that_cannot_present_is_skipped_for_software() {
        let mut gpu = facts("Headless GPU", DeviceKind::DiscreteGpu);
        gpu.surface_compatible = false;
        let list = vec![gpu, facts("lavapipe", DeviceKind::Cpu)];
        let s = select_adapter(&list, LOW).expect("selects");
        assert_eq!(s.index, 1);
        assert!(s.path.is_software());
    }

    #[test]
    fn software_only_refuses_hardware() {
        let list = vec![facts("Apple M2", DeviceKind::IntegratedGpu)];
        let err = select_adapter(&list, AdapterPolicy::SoftwareOnly).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("Apple M2"), "{msg}");
        assert!(msg.contains("SoftwareOnly"), "{msg}");
    }

    #[test]
    fn no_adapter_at_all_is_a_loud_error() {
        let err = select_adapter(&[], LOW).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("no usable GPU or software adapter"), "{msg}");
        assert!(msg.contains("none"), "{msg}");
    }

    #[test]
    fn ties_keep_enumeration_order() {
        let list = vec![
            facts("GPU A", DeviceKind::IntegratedGpu),
            facts("GPU B", DeviceKind::IntegratedGpu),
        ];
        let s = select_adapter(&list, LOW).expect("selects");
        assert_eq!(s.index, 0);
    }
}
