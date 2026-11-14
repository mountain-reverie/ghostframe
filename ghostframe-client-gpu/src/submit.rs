// SPDX-License-Identifier: MIT OR Apache-2.0
//! The submit counter lives in `ghostframe-render`, which makes the flush's
//! submits; this crate's export ring and `Renderer` count theirs through the
//! same per-thread counter, so `Renderer::last_flush_submits` sees all of them.

pub(crate) use ghostframe_render::submit::{count, submit};
