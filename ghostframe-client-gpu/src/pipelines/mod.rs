// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tile codec pipelines that run the real, shipped WGSL against the
//! client's persistent [`crate::framebuffer::Framebuffer`].
//!
//! Each pipeline here loads its shader with `include_str!` from
//! `shaders/client/`, the directory both the web client and this native client
//! compile the exact same file from. The portable ones live in
//! `ghostframe-render` and are re-exported at their old paths.

pub mod cdf53_grid;
pub mod cdf53_passes;
pub use ghostframe_render::pipelines::{cdf53_state, palrle, rgba, solid};
pub mod h264_nv12;
pub mod rgba_slab;
