//! Tile codec pipelines that run the real, shipped WGSL against the
//! client's persistent [`crate::framebuffer::Framebuffer`].
//!
//! Each pipeline here loads its shader with `include_str!` from
//! `shaders/client/`, the directory Task 9 moved the WGSL into precisely so
//! both the web client and this native client compile the exact same file.

pub mod raw;
pub mod solid;
