//! Acked-reference H.264 stream model (design: Phase B).
//!
//! Pure code only: no encoder, no I/O. `syntax` writes and parses slice
//! headers and models what a decoder really references; `tracker` is the
//! per-frame state ring; `refs` is the `RefManager` that decides each frame's
//! plan.

pub(crate) mod backend;
pub(crate) mod param_sets;
pub(crate) mod refs;
pub(crate) mod syntax;
pub(crate) mod tracker;
pub(crate) mod va_sys;
pub(crate) mod vaapi_direct;

#[cfg(test)]
mod refs_tests;
#[cfg(test)]
mod vaapi_direct_tests;
