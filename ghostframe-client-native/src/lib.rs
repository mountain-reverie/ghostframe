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

pub mod event;
pub mod input;
