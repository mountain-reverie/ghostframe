//! Acked-reference H.264 stream model (design: Phase B).
//!
//! Pure code only: no encoder, no I/O. `syntax` writes and parses slice
//! headers and models what a decoder really references; `tracker` is the
//! per-frame state ring; `refs` is the `RefManager` that decides each frame's
//! plan. `pcm_writer` and `pcm_oracle_tests` (test builds only) write real
//! streams from those plans and decode them with libavcodec (design T2).

pub(crate) mod refs;
pub(crate) mod syntax;
pub(crate) mod tracker;

#[cfg(test)]
mod pcm_oracle_tests;
#[cfg(test)]
mod pcm_writer;
#[cfg(test)]
mod refs_tests;
