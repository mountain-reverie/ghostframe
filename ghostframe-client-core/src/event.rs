use ghostframe_protocol::protocol::Codec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DecodeErrorCode {
    PayloadTooShort = 1,
    CountOutOfRange = 2,
    ThinUncachedPalette = 3,
    BundledTruncated = 4,
    IndexOob = 5,
    RleOvershoot = 6,
    RleUndershoot = 7,
    Cdf53BadPass = 8,
    Cdf53Truncated = 9,
    Cdf53RleLength = 10,
}

/// What a completed tile actually hands the GPU.
///
/// The four codecs produce genuinely different things, so this is an enum
/// rather than a byte slice plus a codec tag the consumer has to interpret.
/// `PalRle` and `Cdf53` arrive prevalidated: `reassembly.rs` computes these
/// products anyway, to drive palette state and pass coverage, and emitting
/// the raw wire bytes instead forced every consumer to redo that work.
#[derive(Debug, Clone, PartialEq)]
pub enum TileData {
    /// BGRA wire bytes, unswizzled. Length is payload-proportional
    /// (<= 4096, a multiple of 4).
    Raw(Vec<u8>),
    /// One BGRA quad, expanded to the full tile by the shader.
    Solid([u8; 4]),
    /// `indices` is 512 bytes: two 4-bit palette indices per byte, low
    /// nibble first. The palette itself arrives separately as
    /// `Event::PaletteUpdated` — the GPU upload path never reads an upsert
    /// from here.
    PalRle {
        palette_id: u8,
        count: u8,
        indices: Vec<u8>,
    },
    /// `bit_planes` is 384 bytes: 3 channels x 128, packed B, G, R.
    Cdf53 { pass_idx: u8, bit_planes: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Fully decoded 32x32 RGBA tile, RGBA byte order. For
    /// `Codec::Solid`/`PalRle`/`Cdf53` this is always 4096 bytes (32*32*4);
    /// for `Codec::Raw` the length is payload-proportional (<= 4096, a
    /// multiple of 4), mirroring the TS renderer contract.
    TileReady {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        rgba: Vec<u8>,
    },
    FrameDimensions {
        width: u32,
        height: u32,
    },
    /// Complete H.264 access unit — platform decodes (WebCodecs / ffmpeg).
    NeedsH264 {
        frame_seq: u32,
        timestamp_us: u32,
        is_keyframe: bool,
        payload: Vec<u8>,
    },
    DecodeError {
        codec: Codec,
        tile_x: u8,
        tile_y: u8,
        code: DecodeErrorCode,
    },
    /// A reassembled, parity-recovered, prevalidated, generation-checked tile
    /// pass — but NOT decoded. Emitted instead of `TileReady` when
    /// `TileDelivery::Payload` is configured, so a GPU decoder can take it.
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        /// Tile generation, for superseding. Stays on the event rather than
        /// in `TileData`: superseding is codec-independent.
        generation: u8,
        data: TileData,
    },
    /// A palette slot changed. Only emitted under `TileDelivery::Payload`:
    /// under `Decoded` the core applies the palette itself and the consumer
    /// never needs to see it.
    ///
    /// Colours are BGRA, matching the wire and the palette table.
    PaletteUpdated {
        palette_id: u8,
        colors: Vec<[u8; 4]>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PollOutput {
    /// Send as a QUIC/WebTransport datagram (ACK batches, NACK envelopes).
    Datagram(Vec<u8>),
    /// Send on the bidirectional feedback stream (Hello, ReceiverFeedback, DecodeError).
    Stream(Vec<u8>),
}
