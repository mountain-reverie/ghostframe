//! Acked-reference H.264 stream model (design: Phase B).
//!
//! Pure code only: no encoder, no I/O. `syntax` writes and parses slice
//! headers and models what a decoder really references.

pub(crate) mod syntax;
