//! GPU decode and dmabuf export for the native ghostframe client.
//!
//! ## Catch-all match arms
//!
//! `clippy::wildcard_enum_match_arm` is on for non-test code in this crate.
//! A `_ =>` arm that silently absorbs unknown variants has twice hidden real
//! defects here: a wrong classifier that stood for three months, and inbound
//! routing that fed every non-NACK datagram to the ACK parser. Listing the
//! variants makes adding one a compile error at every site that must decide
//! about it.
//!
//! Wildcards that fail *loudly* (`other => panic!(...)` in a test) are fine,
//! which is why the lint is scoped to `not(test)`. A wildcard that is
//! genuinely required -- a foreign enum we do not control -- takes a local
//! `#[allow]` with a reason, so every exception is a decision on the record.
#![cfg_attr(not(test), warn(clippy::wildcard_enum_match_arm))]

pub mod coalesce;
pub mod config;
pub mod dirty;
pub mod export;
pub mod framebuffer;
pub mod wgpu_ctx;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GpuError {
    #[error("vulkan: {0}")]
    Vulkan(String),

    /// No Vulkan adapter at all -- missing loader, no ICD, or a broken driver
    /// install. Distinct from [`GpuError::AdapterCannotExport`] so the message
    /// does not send someone hunting for extension support when the real
    /// problem is that Vulkan is not working.
    #[error("no Vulkan adapter found (is a Vulkan driver installed?)")]
    NoVulkanAdapter,

    /// An adapter exists but cannot export memory as a file descriptor, so it
    /// can never produce a dmabuf. The feature name is spelled out because it
    /// is greppable against `vulkaninfo` output.
    #[error(
        "adapter {adapter:?} lacks VK_KHR_external_memory_fd \
         (wgpu Features::VULKAN_EXTERNAL_MEMORY_FD), so it cannot export a dmabuf"
    )]
    AdapterCannotExport { adapter: String },

    /// The device and the consumer share no DRM format modifier.
    ///
    /// Both lists are carried because modifier debugging is precisely "which
    /// set did each side offer", and an error that omits them sends the reader
    /// back to reproduce it with logging on.
    #[error(
        "no DRM format modifier supported by both this device ({device:?}) \
         and the consumer ({requested:?})"
    )]
    NoCommonModifier {
        device: Vec<u64>,
        requested: Vec<u64>,
    },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
