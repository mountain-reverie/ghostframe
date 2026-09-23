//! Native client library foundation for `ghostframe-client-capi`.
//!
//! This crate will own the tsnet transport, a net thread and a render
//! thread, and expose an idiomatic Rust API that `ghostframe-client-capi`
//! wraps in a C ABI. For now it provides two self-contained pieces of
//! that foundation:
//!
//! - [`event`]: an eventfd-backed queue the host adds to its own
//!   `poll`/`epoll` set to learn when the library has something to
//!   report.
//! - [`input`]: thin wrappers over `ghostframe_client_core::input` for
//!   encoding input events onto the wire.

pub mod bootstrap;
pub mod event;
pub mod input;

/// Errors surfaced by this crate's public API.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// A ghostbridge/tsnet FFI call failed (connect, dial, up, ...).
    #[error("tailnet: {0}")]
    Bridge(#[from] ghostframe_tsnet::GhostbridgeError),
    /// A local I/O failure: fd setup, read/write on the tsnet socketpair,
    /// eventfd/epoll/timerfd syscalls.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The `/config.json` cert-hash bootstrap request failed or was
    /// malformed.
    #[error("bootstrap: {0}")]
    Bootstrap(String),
    /// The QUIC/WebTransport transport layer failed.
    #[error("transport: {0}")]
    Net(#[from] ghostframe_client_net::ClientNetError),
    /// GPU/renderer construction or operation failed.
    #[error("gpu: {0}")]
    Gpu(#[from] ghostframe_client_gpu::GpuError),
}
