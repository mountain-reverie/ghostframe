// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser client: startup, WebTransport I/O, the rAF loop, H.264 decode,
//! input and overlays (spec section 5.4). What decides things is plain Rust,
//! unit-tested on the host; what touches the browser is wasm32-only.

pub mod batch;
pub mod certs;
pub mod close;
pub mod decoder;
pub mod input_map;
pub mod page;
pub mod session;

#[cfg(target_arch = "wasm32")]
pub mod bootstrap;
#[cfg(target_arch = "wasm32")]
pub mod debug_api;
#[cfg(target_arch = "wasm32")]
pub mod dom;
#[cfg(target_arch = "wasm32")]
pub mod hooks;
#[cfg(target_arch = "wasm32")]
pub mod js;
#[cfg(target_arch = "wasm32")]
pub mod streams;
#[cfg(target_arch = "wasm32")]
pub mod webcodecs;
