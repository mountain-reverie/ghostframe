// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser renderer. The parts that decide things (`limits`, `logic`,
//! `route`) are plain Rust and unit-tested on the host; everything that
//! touches WebGPU or the DOM is wasm32-only.

pub mod limits;
pub mod logic;
pub mod route;
