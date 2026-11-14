// SPDX-License-Identifier: MIT OR Apache-2.0
//! The portable tile renderer shared by the native and the web client.
//!
//! The framebuffer, the Solid / PalRLE / RGBA / CDF 5/3 pipelines, the
//! paint-order ledger and the damage bookkeeping, extracted from
//! `ghostframe-client-gpu`. The crate builds for `wasm32-unknown-unknown`: no
//! threads, no blocking waits (except in the native-only helpers marked as
//! such), no `std::time::Instant`. Time comes in as `now_us`.
//!
//! ## Catch-all match arms
//!
//! `clippy::wildcard_enum_match_arm` is on for non-test code, as in
//! `ghostframe-client-gpu`: a `_ =>` arm that silently absorbs a new variant
//! hid real defects twice. List the variants, so adding one is a compile error
//! at every site that must decide about it.
#![cfg_attr(not(test), warn(clippy::wildcard_enum_match_arm))]

pub mod coalesce;
pub mod dirty;
pub mod framebuffer;
pub(crate) mod paint_order;
pub mod pipelines;
pub mod readback;
pub mod submit;
pub mod tile_renderer;

#[cfg(test)]
mod noop_device;

#[cfg(not(target_arch = "wasm32"))]
pub use tile_renderer::GpuDecodeError;
pub use tile_renderer::{
    dimensions_valid, tile_grid, DebugState, DebugTile, FlushReport, NoSlots, SlotSink, SlotTile,
    TileRenderer,
};

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
