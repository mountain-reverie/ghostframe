// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser renderer. The parts that decide things (`limits`, `logic`,
//! `route`) are plain Rust and unit-tested on the host; everything that
//! touches WebGPU or the DOM is wasm32-only.

#[cfg(target_arch = "wasm32")]
pub mod h264;
pub mod limits;
pub mod logic;
#[cfg(target_arch = "wasm32")]
pub mod raw;
pub mod route;
