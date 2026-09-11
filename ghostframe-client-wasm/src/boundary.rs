//! Serde mirrors of `client-core`'s `Event` and `PollOutput`.
//!
//! `client-core` is consumed by `ghostframe-lib` and `ghostframe-e2e` on
//! native targets; neither should acquire `serde` or `wasm-bindgen` in its
//! graph just so the browser can have JSON-shaped events. The mirror lives
//! here instead.
//!
//! Serialised with `#[serde(tag = "kind")]`, so JS sees plain objects like
//! `{ kind: 'TilePayload', tile_x: 3, payload: Uint8Array }`.

use ghostframe_client_core::{
    cdf53_coverage::{ArrivalOutcome, CoverageEntry},
    cdf53_prevalidate::PrevalidatedCdf53,
    pal_rle_decode::{PalRleVariant, PrevalidatedPalRle},
    Event, PollOutput,
};
use ghostframe_protocol::ack::AckEntry;
use serde::Serialize;

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind")]
pub enum WasmEvent {
    TileReady {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        rgba: Vec<u8>,
    },
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pass_idx: u8,
        generation: u8,
        /// `Codec` discriminant; `Codec` lives in `ghostframe-protocol` and
        /// is not `Serialize`, so it crosses as its `repr(u8)` value.
        codec: u8,
        payload: Vec<u8>,
    },
    PaletteUpdated {
        palette_id: u8,
        colors: Vec<[u8; 4]>,
    },
    FrameDimensions {
        width: u32,
        height: u32,
    },
    NeedsH264 {
        frame_seq: u32,
        timestamp_us: u32,
        is_keyframe: bool,
        payload: Vec<u8>,
    },
    DecodeError {
        codec: u8,
        tile_x: u8,
        tile_y: u8,
        /// `DecodeErrorCode` discriminant, 1..=10.
        code: u8,
    },
}

impl From<&Event> for WasmEvent {
    fn from(ev: &Event) -> Self {
        match ev {
            Event::TileReady {
                frame_seq,
                tile_x,
                tile_y,
                rgba,
            } => WasmEvent::TileReady {
                frame_seq: *frame_seq,
                tile_x: *tile_x,
                tile_y: *tile_y,
                rgba: rgba.clone(),
            },
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                generation,
                codec,
                payload,
            } => WasmEvent::TilePayload {
                frame_seq: *frame_seq,
                tile_x: *tile_x,
                tile_y: *tile_y,
                pass_idx: *pass_idx,
                generation: *generation,
                codec: *codec as u8,
                payload: payload.clone(),
            },
            Event::PaletteUpdated { palette_id, colors } => WasmEvent::PaletteUpdated {
                palette_id: *palette_id,
                colors: colors.clone(),
            },
            Event::FrameDimensions { width, height } => WasmEvent::FrameDimensions {
                width: *width,
                height: *height,
            },
            Event::NeedsH264 {
                frame_seq,
                timestamp_us,
                is_keyframe,
                payload,
            } => WasmEvent::NeedsH264 {
                frame_seq: *frame_seq,
                timestamp_us: *timestamp_us,
                is_keyframe: *is_keyframe,
                payload: payload.clone(),
            },
            Event::DecodeError {
                codec,
                tile_x,
                tile_y,
                code,
            } => WasmEvent::DecodeError {
                codec: *codec as u8,
                tile_x: *tile_x,
                tile_y: *tile_y,
                code: *code as u8,
            },
        }
    }
}

/// Which wire an outbound buffer belongs on. The browser sends datagrams via
/// `transport.datagrams.writable` and stream bytes via the bidi stream
/// writer; it must not conflate them.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind")]
pub enum WasmPollOutput {
    Datagram { bytes: Vec<u8> },
    Stream { bytes: Vec<u8> },
}

impl From<PollOutput> for WasmPollOutput {
    fn from(out: PollOutput) -> Self {
        match out {
            PollOutput::Datagram(bytes) => WasmPollOutput::Datagram { bytes },
            PollOutput::Stream(bytes) => WasmPollOutput::Stream { bytes },
        }
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct WasmAckEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    pub arrival_time_ms_lo16: u16,
}

impl From<&AckEntry> for WasmAckEntry {
    fn from(e: &AckEntry) -> Self {
        WasmAckEntry {
            frame_seq: e.frame_seq,
            tile_x: e.tile_x,
            tile_y: e.tile_y,
            pass_idx: e.pass_idx,
            arrival_time_ms_lo16: e.arrival_time_ms_lo16,
        }
    }
}

/// A prevalidation outcome. `ok: false` carries the `DecodeErrorCode`
/// discriminant in `code`; `ok: true` carries the payload. Modelled as one
/// struct rather than a tagged enum so the suites can write
/// `expect(r.ok).toBe(false); expect(r.code).toBe(3)` without narrowing.
#[derive(Debug, Serialize)]
pub struct WasmPrevalidatedPalRle {
    pub ok: bool,
    pub code: u8,
    /// 0 = Bundled, 1 = Thin, 2 = IndicesRaw.
    pub variant: u8,
    pub palette_id: u8,
    pub count: u8,
    /// 512 bytes, 2 pixels/byte, low nibble first. Empty when `ok` is false.
    pub indices: Vec<u8>,
    /// `count * 4` BGRA bytes for Bundled; empty otherwise.
    pub palette_upsert: Vec<u8>,
    /// Distinguishes "Bundled with an empty upsert" from "not Bundled".
    pub has_palette_upsert: bool,
}

impl From<Result<PrevalidatedPalRle, ghostframe_client_core::DecodeErrorCode>>
    for WasmPrevalidatedPalRle
{
    fn from(r: Result<PrevalidatedPalRle, ghostframe_client_core::DecodeErrorCode>) -> Self {
        match r {
            Ok(p) => WasmPrevalidatedPalRle {
                ok: true,
                code: 0,
                variant: match p.variant {
                    PalRleVariant::Bundled => 0,
                    PalRleVariant::Thin => 1,
                    PalRleVariant::IndicesRaw => 2,
                },
                palette_id: p.palette_id,
                count: p.count,
                indices: p.indices,
                has_palette_upsert: p.palette_upsert.is_some(),
                palette_upsert: p.palette_upsert.unwrap_or_default(),
            },
            Err(code) => WasmPrevalidatedPalRle {
                ok: false,
                code: code as u8,
                variant: 0,
                palette_id: 0,
                count: 0,
                indices: Vec::new(),
                palette_upsert: Vec::new(),
                has_palette_upsert: false,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WasmPrevalidatedCdf53 {
    pub ok: bool,
    pub code: u8,
    pub generation: u8,
    pub pass_idx: u8,
    /// 384 bytes = 3 channels x 128, packed B, G, R. Empty when `ok` is false.
    pub bit_planes: Vec<u8>,
}

impl From<Result<PrevalidatedCdf53, ghostframe_client_core::DecodeErrorCode>>
    for WasmPrevalidatedCdf53
{
    fn from(r: Result<PrevalidatedCdf53, ghostframe_client_core::DecodeErrorCode>) -> Self {
        match r {
            Ok(p) => WasmPrevalidatedCdf53 {
                ok: true,
                code: 0,
                generation: p.generation,
                pass_idx: p.pass_idx,
                bit_planes: p.bit_planes,
            },
            Err(code) => WasmPrevalidatedCdf53 {
                ok: false,
                code: code as u8,
                generation: 0,
                pass_idx: 0,
                bit_planes: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WasmCoverageEntry {
    pub generation: u8,
    pub frame_seq: u32,
    pub pass_mask: u16,
    pub nacked_mask: u16,
    pub last_change_us: u64,
}

impl From<CoverageEntry> for WasmCoverageEntry {
    fn from(e: CoverageEntry) -> Self {
        WasmCoverageEntry {
            generation: e.generation,
            frame_seq: e.frame_seq,
            pass_mask: e.pass_mask,
            nacked_mask: e.nacked_mask,
            last_change_us: e.last_change_us,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WasmArrivalOutcome {
    pub entry: WasmCoverageEntry,
    pub nack_passes: Vec<u8>,
}

impl From<ArrivalOutcome> for WasmArrivalOutcome {
    fn from(o: ArrivalOutcome) -> Self {
        WasmArrivalOutcome {
            entry: o.entry.into(),
            nack_passes: o.nack_passes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_client_core::{DecodeErrorCode, Event};
    use ghostframe_protocol::protocol::Codec;

    #[test]
    fn tile_payload_mirrors_every_field() {
        let ev = Event::TilePayload {
            frame_seq: 0x1234_5678,
            tile_x: 3,
            tile_y: 7,
            pass_idx: 13,
            generation: 2,
            codec: Codec::Cdf53,
            payload: vec![1, 2, 3],
        };
        match WasmEvent::from(&ev) {
            WasmEvent::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                generation,
                codec,
                payload,
            } => {
                assert_eq!(frame_seq, 0x1234_5678);
                assert_eq!(tile_x, 3);
                assert_eq!(tile_y, 7);
                assert_eq!(pass_idx, 13);
                assert_eq!(generation, 2);
                assert_eq!(codec, Codec::Cdf53 as u8);
                assert_eq!(payload, vec![1, 2, 3]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn decode_error_carries_the_code_discriminant() {
        let ev = Event::DecodeError {
            codec: Codec::PalRle,
            tile_x: 1,
            tile_y: 2,
            code: DecodeErrorCode::IndexOob,
        };
        match WasmEvent::from(&ev) {
            WasmEvent::DecodeError { code, .. } => assert_eq!(code, 5),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn palette_updated_preserves_colour_order() {
        let ev = Event::PaletteUpdated {
            palette_id: 9,
            colors: vec![[1, 2, 3, 4], [5, 6, 7, 8]],
        };
        match WasmEvent::from(&ev) {
            WasmEvent::PaletteUpdated { palette_id, colors } => {
                assert_eq!(palette_id, 9);
                assert_eq!(colors, vec![[1, 2, 3, 4], [5, 6, 7, 8]]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
