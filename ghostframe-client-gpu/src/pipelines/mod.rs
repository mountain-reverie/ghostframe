//! Tile codec pipelines that run the real, shipped WGSL against the
//! client's persistent [`crate::framebuffer::Framebuffer`].
//!
//! Each pipeline here loads its shader with `include_str!` from
//! `shaders/client/`, the directory Task 9 moved the WGSL into precisely so
//! both the web client and this native client compile the exact same file.

pub mod cdf53;
pub mod cdf53_passes;
pub mod h264_nv12;
pub mod palrle;
pub mod raw;
pub mod solid;
