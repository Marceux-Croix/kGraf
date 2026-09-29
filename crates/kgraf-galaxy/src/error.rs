//! Library errors for the galaxy renderer.

/// Everything the renderer can refuse.
#[derive(Debug, thiserror::Error)]
pub enum GalaxyError {
    /// The scene violates its contract (bad index, non-finite value).
    #[error("invalid scene: {0}")]
    InvalidScene(String),
    /// The render target size is zero or beyond the device limit.
    #[error("invalid render target size {width}x{height} (device max {max})")]
    InvalidTarget { width: u32, height: u32, max: u32 },
    /// The scene needs a GPU buffer or draw beyond the device's limit. The graph is
    /// not drawn partially; the caller shows this instead.
    #[error("scene too large for the GPU: {what} needs {bytes} (device limit {max})")]
    SceneTooLarge {
        what: &'static str,
        bytes: u64,
        max: u64,
    },
    /// Reading a rendered frame back failed.
    #[error("frame readback failed: {0}")]
    Readback(String),
}
