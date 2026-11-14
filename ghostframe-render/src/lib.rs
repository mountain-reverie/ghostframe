// SPDX-License-Identifier: MIT OR Apache-2.0
//! The portable tile renderer shared by the native and the web client.
//!
//! Scaffold: the modules arrive in the following commits.

use thiserror::Error;

/// What can go wrong in this crate. `ghostframe-client-gpu`'s own `GpuError`
/// converts from it (`From`), so its callers keep matching the variants they
/// always matched.
#[derive(Debug, Error)]
pub enum GpuError {
    /// The device failed or was lost while the renderer waited on it (a
    /// buffer map or a poll). Carried as an error so the render thread can
    /// surface it instead of panicking.
    #[error("gpu device: {0}")]
    Device(String),

    /// A frame size outside `1..=MAX_FRAME_DIMENSION` per axis. The value
    /// originates on the network, so it is refused before any texture or
    /// buffer is sized from it.
    #[error(
        "frame dimensions {width}x{height} are outside 1..={max} per axis",
        max = ghostframe_client_core::MAX_FRAME_DIMENSION
    )]
    InvalidDimensions { width: u32, height: u32 },

    /// `TileRenderer::new` was given a framebuffer format the decode shaders
    /// cannot write. They bind the framebuffer as a `rgba8unorm` storage
    /// texture, which no other format (the canvas's preferred `bgra8unorm`
    /// included) can stand in for.
    #[error(
        "framebuffer format {0:?} is not supported: the decode shaders write an rgba8unorm storage texture"
    )]
    UnsupportedFormat(wgpu::TextureFormat),
}
