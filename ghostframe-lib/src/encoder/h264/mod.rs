//! Acked-reference H.264 stream model (design: Phase B).
//!
//! Pure code only: no encoder, no I/O. `syntax` writes and parses slice
//! headers and models what a decoder really references; `tracker` is the
//! per-frame state ring; `refs` is the `RefManager` that decides each frame's
//! plan.

pub(crate) mod refs;
pub(crate) mod syntax;
pub(crate) mod tracker;

#[cfg(test)]
mod refs_tests;
