// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser client: startup, WebTransport I/O, the rAF loop, H.264 decode,
//! input and overlays (spec section 5.4). What decides things is plain Rust,
//! unit-tested on the host; what touches the browser is wasm32-only.

pub mod batch;
pub mod certs;
pub mod close;
pub mod page;
