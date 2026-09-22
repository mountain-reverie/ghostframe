//! GPU decode and dmabuf export for the native ghostframe client.
//!
//! ## Catch-all match arms
//!
//! `clippy::wildcard_enum_match_arm` is on for non-test code, matching
//! `ghostframe-client-core`. A `_ =>` arm that silently absorbs unknown
//! variants has twice hidden real defects in this tree; listing variants
//! makes adding one a compile error at every site that must decide.
#![cfg_attr(not(test), warn(clippy::wildcard_enum_match_arm))]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GpuError {
    #[error("vulkan: {0}")]
    Vulkan(String),
    #[error("no adapter supports the required dmabuf export extensions")]
    NoSuitableAdapter,
    #[error("no DRM format modifier is supported by both this device and the consumer")]
    NoCommonModifier,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
