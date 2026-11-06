//! Acked-reference H.264 stream model (design: Phase B).
//!
//! Pure code only: no encoder, no I/O. `syntax` writes and parses slice
//! headers and models what a decoder really references; `tracker` is the
//! per-frame state ring; `refs` is the `RefManager` that decides each frame's
//! plan. `pcm_writer` and `pcm_decoder` write real streams from those plans
//! and decode them with libavcodec; `pcm_oracle_tests` (test builds only) is
//! design T2, and `harness` (the `browserless-harness` feature) runs them
//! behind the transport for the browserless scenes (design T3).

pub(crate) mod backend;
#[cfg(feature = "browserless-harness")]
pub mod harness;
pub(crate) mod param_sets;
pub(crate) mod refs;
pub(crate) mod syntax;
pub(crate) mod tracker;
pub(crate) mod va_sys;
pub(crate) mod vaapi_direct;

#[cfg(test)]
mod openh264_fixture_tests;
#[cfg(any(test, feature = "browserless-harness"))]
pub(crate) mod pcm_decoder;
#[cfg(test)]
mod pcm_oracle_tests;
#[cfg(any(test, feature = "browserless-harness"))]
pub(crate) mod pcm_rate;
#[cfg(any(test, feature = "browserless-harness"))]
pub(crate) mod pcm_writer;
#[cfg(test)]
mod refs_tests;
#[cfg(test)]
pub(crate) mod vaapi_direct_tests;
