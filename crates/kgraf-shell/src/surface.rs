//! Dropped-frame accounting.
//!
//! egui-wgpu runs `App::ui` first and acquires the surface texture afterwards. When the
//! acquire fails (the window is still hidden, covered, resized, or the GPU timed out),
//! the frame is skipped after the UI already ran. Without this module those skipped
//! frames were counted as rendered: one run had 438 of 504 `ui` calls skipped.
//!
//! egui-wgpu calls `WgpuConfiguration::on_surface_status` for every acquire that is not
//! `Success` or `Suboptimal`. We install our own callback that counts the drop by kind
//! and then returns the same action egui-wgpu's default callback would.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use egui_wgpu::SurfaceErrorAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropKind {
    Timeout,
    Occluded,
    Outdated,
    Lost,
    Other,
}

/// Which drop, if any, a surface status means. Pure.
pub fn drop_kind(status: &wgpu::CurrentSurfaceTexture) -> Option<DropKind> {
    match status {
        wgpu::CurrentSurfaceTexture::Success(_) | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
            None
        }
        wgpu::CurrentSurfaceTexture::Timeout => Some(DropKind::Timeout),
        wgpu::CurrentSurfaceTexture::Occluded => Some(DropKind::Occluded),
        wgpu::CurrentSurfaceTexture::Outdated => Some(DropKind::Outdated),
        wgpu::CurrentSurfaceTexture::Lost => Some(DropKind::Lost),
        #[allow(unreachable_patterns)] // wgpu may add variants; count them, do not panic.
        _ => Some(DropKind::Other),
    }
}

/// The recovery egui-wgpu 0.36's default callback takes for each kind. Pure.
pub fn recovery(kind: DropKind) -> SurfaceErrorAction {
    match kind {
        // Must reconfigure before the next acquire or it stays Outdated forever.
        DropKind::Outdated => SurfaceErrorAction::Reconfigure,
        DropKind::Lost => SurfaceErrorAction::RecreateSurface,
        DropKind::Timeout | DropKind::Occluded | DropKind::Other => SurfaceErrorAction::SkipFrame,
    }
}

/// Drop counts shared between the render callback and the app.
#[derive(Debug, Default)]
pub struct DropCounters {
    timeout: AtomicU64,
    occluded: AtomicU64,
    outdated: AtomicU64,
    lost: AtomicU64,
    other: AtomicU64,
}

/// A copy of the counters at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DropSnapshot {
    pub timeout: u64,
    pub occluded: u64,
    pub outdated: u64,
    pub lost: u64,
    pub other: u64,
}

impl DropSnapshot {
    pub fn total(&self) -> u64 {
        self.timeout + self.occluded + self.outdated + self.lost + self.other
    }
}

impl DropCounters {
    pub fn record(&self, kind: DropKind) {
        let counter = match kind {
            DropKind::Timeout => &self.timeout,
            DropKind::Occluded => &self.occluded,
            DropKind::Outdated => &self.outdated,
            DropKind::Lost => &self.lost,
            DropKind::Other => &self.other,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> DropSnapshot {
        DropSnapshot {
            timeout: self.timeout.load(Ordering::Relaxed),
            occluded: self.occluded.load(Ordering::Relaxed),
            outdated: self.outdated.load(Ordering::Relaxed),
            lost: self.lost.load(Ordering::Relaxed),
            other: self.other.load(Ordering::Relaxed),
        }
    }
}

/// Builds the `on_surface_status` callback: count, log, then recover as egui-wgpu would.
pub fn status_callback(
    counters: Arc<DropCounters>,
) -> Arc<dyn Fn(&wgpu::CurrentSurfaceTexture) -> SurfaceErrorAction + Send + Sync> {
    Arc::new(move |status| {
        let kind = drop_kind(status).unwrap_or(DropKind::Other);
        counters.record(kind);
        match kind {
            // Expected while hidden or resizing; counted, logged quietly.
            DropKind::Occluded | DropKind::Outdated => {
                log::debug!("dropped frame: {kind:?}");
            }
            DropKind::Timeout | DropKind::Lost | DropKind::Other => {
                log::warn!("dropped frame: {kind:?}");
            }
        }
        recovery(kind)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_status_maps_to_its_kind() {
        use wgpu::CurrentSurfaceTexture as S;
        assert_eq!(drop_kind(&S::Timeout), Some(DropKind::Timeout));
        assert_eq!(drop_kind(&S::Occluded), Some(DropKind::Occluded));
        assert_eq!(drop_kind(&S::Outdated), Some(DropKind::Outdated));
        assert_eq!(drop_kind(&S::Lost), Some(DropKind::Lost));
    }

    #[test]
    fn recovery_matches_egui_wgpu_defaults() {
        assert!(matches!(
            recovery(DropKind::Outdated),
            SurfaceErrorAction::Reconfigure
        ));
        assert!(matches!(
            recovery(DropKind::Lost),
            SurfaceErrorAction::RecreateSurface
        ));
        for k in [DropKind::Timeout, DropKind::Occluded, DropKind::Other] {
            assert!(matches!(recovery(k), SurfaceErrorAction::SkipFrame));
        }
    }

    #[test]
    fn callback_counts_by_kind() {
        let counters = Arc::new(DropCounters::default());
        let cb = status_callback(Arc::clone(&counters));
        cb(&wgpu::CurrentSurfaceTexture::Occluded);
        cb(&wgpu::CurrentSurfaceTexture::Occluded);
        cb(&wgpu::CurrentSurfaceTexture::Timeout);
        let snap = counters.snapshot();
        assert_eq!(snap.occluded, 2);
        assert_eq!(snap.timeout, 1);
        assert_eq!(snap.total(), 3);
    }
}
