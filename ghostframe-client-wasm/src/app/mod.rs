// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser client: startup, WebTransport I/O, the rAF loop, H.264 decode,
//! input and overlays (spec section 5.4). What decides things is plain Rust,
//! unit-tested on the host; what touches the browser is wasm32-only.

pub mod batch;
pub mod capped;
pub mod certs;
pub mod close;
pub mod decoder;
pub mod health;
pub mod hooks;
pub mod input_map;
pub mod page;
pub mod recorder;
pub mod repair_status;
pub mod session;
pub mod stats_line;

#[cfg(target_arch = "wasm32")]
pub mod bootstrap;
#[cfg(target_arch = "wasm32")]
pub mod client;
#[cfg(target_arch = "wasm32")]
pub mod debug_hooks;
#[cfg(target_arch = "wasm32")]
pub mod dom;
#[cfg(target_arch = "wasm32")]
pub mod input;
#[cfg(target_arch = "wasm32")]
pub mod js;
#[cfg(target_arch = "wasm32")]
pub mod streams;
#[cfg(target_arch = "wasm32")]
pub mod webcodecs;
