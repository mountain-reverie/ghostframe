// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tile codec pipelines that run the real, shipped WGSL against the
//! persistent [`crate::framebuffer::Framebuffer`].
//!
//! Each pipeline here loads its shader with `include_str!` from
//! `shaders/client/`, the directory both the web client and the native client
//! compile the exact same files from.

pub mod cdf53_grid;
pub mod cdf53_state;
pub mod palrle;
pub mod rgba;
pub mod solid;
