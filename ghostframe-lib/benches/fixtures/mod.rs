//! Shared bench fixtures: content classes and the BenchEncoder trait.
//!
//! M3 codec implementations land by adding a new `BenchEncoder` impl in this
//! module (or a sibling file) — no changes needed in the bench files
//! themselves.

// Not all items are used under every feature combination; suppress noise.
#![allow(dead_code)]

#[allow(dead_code)]
pub mod flat_ui;
#[allow(dead_code)]
pub mod gradient;
#[allow(dead_code)]
pub mod motion;
#[allow(dead_code)]
pub mod photo;
#[allow(dead_code)]
pub mod solid;
#[allow(dead_code)]
pub mod text;

/// Edge length of a single tile, in pixels. Mirrors `ghostframe_lib::tile::TILE_SIZE`.
pub const TILE_SIZE: u32 = 32;

/// Bytes per pixel — BGRA8.
pub const BPP: u32 = 4;

/// Raw tile size in bytes.
pub const TILE_BYTES: usize = (TILE_SIZE * TILE_SIZE * BPP) as usize;

/// One representative tile per content class.
/// All buffers are exactly `TILE_BYTES` long, BGRA8.
#[derive(Debug, Clone, Copy)]
pub enum ContentClass {
    Solid,
    FlatUi,
    Text,
    Gradient,
    Photo,
    Motion,
}

impl ContentClass {
    pub const ALL: &'static [ContentClass] = &[
        ContentClass::Solid,
        ContentClass::FlatUi,
        ContentClass::Text,
        ContentClass::Gradient,
        ContentClass::Photo,
        ContentClass::Motion,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ContentClass::Solid => "solid",
            ContentClass::FlatUi => "flat_ui",
            ContentClass::Text => "text",
            ContentClass::Gradient => "gradient",
            ContentClass::Photo => "photo",
            ContentClass::Motion => "motion",
        }
    }

    /// Returns the BGRA tile buffer for this content class.
    pub fn tile(self) -> Vec<u8> {
        match self {
            ContentClass::Solid => solid::tile(),
            ContentClass::FlatUi => flat_ui::tile(),
            ContentClass::Text => text::tile(),
            ContentClass::Gradient => gradient::tile(),
            ContentClass::Photo => photo::tile(),
            ContentClass::Motion => motion::tile(),
        }
    }
}

/// Trait every bench-able codec implements.
///
/// One instance per codec is created up front; the bench loop calls `encode`
/// repeatedly.
///
/// **Display naming contract:** `name()` returns the codec name only (no
/// suffix). Callers constructing bench-id strings or report headers MUST
/// check `lz4()` and append `"+lz4"` themselves when the wrapper is active.
/// Example: `if encoder.lz4() { format!("{}+lz4", encoder.name()) } else { encoder.name().to_string() }`.
/// Returning a dynamic string from `name()` is not possible because the
/// `'static` lifetime would force a leak — callers compose the display
/// name instead.
pub trait BenchEncoder {
    fn name(&self) -> &'static str;
    fn lz4(&self) -> bool {
        false
    }
    fn encode(&mut self, tile: &[u8]) -> Vec<u8>;
}

/// Wraps any `BenchEncoder` in an LZ4 post-pass. Used to measure the
/// per-codec break-even §12 calls for.
pub struct Lz4Wrapper<E: BenchEncoder> {
    inner: E,
}

impl<E: BenchEncoder> Lz4Wrapper<E> {
    pub fn new(inner: E) -> Self {
        Self { inner }
    }
}

impl<E: BenchEncoder> BenchEncoder for Lz4Wrapper<E> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn lz4(&self) -> bool {
        true
    }
    fn encode(&mut self, tile: &[u8]) -> Vec<u8> {
        let raw = self.inner.encode(tile);
        lz4_flex::compress_prepend_size(&raw)
    }
}
