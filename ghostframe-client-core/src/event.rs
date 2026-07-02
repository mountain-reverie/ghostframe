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
}

#[derive(Debug, Clone, PartialEq)]
pub enum PollOutput {
    /// Send as a QUIC/WebTransport datagram (ACK batches, NACK envelopes).
    Datagram(Vec<u8>),
    /// Send on the bidirectional feedback stream (Hello, ReceiverFeedback, DecodeError).
    Stream(Vec<u8>),
}
