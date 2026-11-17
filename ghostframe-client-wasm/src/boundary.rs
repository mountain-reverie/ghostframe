// SPDX-License-Identifier: MIT OR Apache-2.0
//! Serde mirrors of `client-core`'s `Event`.
//!
//! `client-core` is consumed by `ghostframe-lib` and `ghostframe-e2e` on
//! native targets; neither should acquire `serde` or `wasm-bindgen` in its
//! graph just so the browser can have JSON-shaped events. The mirror lives
//! here instead.
//!
//! Serialised with `#[serde(tag = "kind")]`, so JS sees plain objects like
//! `{ kind: 'TilePayload', tile_x: 3, payload: Uint8Array }`.

use ghostframe_client_core::{Event, TileData};
use serde::Serialize;

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(tag = "kind")]
pub enum WasmEvent {
    TileReady {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        #[serde(with = "serde_bytes")]
        rgba: Vec<u8>,
    },
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        generation: u8,
        data: WasmTileData,
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
        /// `frame_seq` this frame predicts from; `0xFFFF_FFFF` for an IDR.
        ref_frame_seq: u32,
        /// `FRAME_FLAG_*` bits of the v2 frame header.
        flags: u8,
        /// Long-term slot indices (bits 0-1 mark, bits 2-3 target).
        slots: u8,
        /// The decoder must be reset before this access unit: it is an IDR,
        /// or the gate was reset since the last frame it fed.
        reset_decoder: bool,
        #[serde(with = "serde_bytes")]
        payload: Vec<u8>,
    },
    DecodeError {
        codec: u8,
        tile_x: u8,
        tile_y: u8,
        /// `DecodeErrorCode` discriminant, 1..=10.
        code: u8,
    },
    /// The server has dropped this session and said why. Terminal: no
    /// further frames will arrive.
    ///
    /// Mirrors `ghostframe_client_core::Event::Evicted` so this crate keeps
    /// compiling against `Event`'s full variant set (this file has no
    /// wildcard arm -- see the module doc's rationale for the other
    /// variants). Surfacing this to the user in the browser UI is web-client
    /// scope, not this boundary crate's.
    Evicted {
        /// `EvictionReason` discriminant (`to_byte()`): `1` for
        /// `DisplacedByNewSession`, or the raw byte for `Unknown(b)`. A
        /// code, not a string -- JS renders its own wording, and the
        /// consumer compares against a numeric constant rather than parsing
        /// a `Debug`-formatted string that is free to be reworded later.
        reason: u8,
    },
}

/// Mirror of `TileData`. Serialised `#[serde(tag = "codec")]`, so JS sees
/// `{ codec: 'PalRle', palette_id, count, indices }` — the codec tag the old
/// flat shape carried separately is now the variant itself.
#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(tag = "codec")]
pub enum WasmTileData {
    Solid {
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    PalRle {
        palette_id: u8,
        count: u8,
        #[serde(with = "serde_bytes")]
        indices: Vec<u8>,
        /// The payload's flags byte as the server encoded it.
        /// `e2e_indices_raw_handshake` reads this off
        /// `window.__ghostframeRecordedFlags` and tests bit 1, so it must be
        /// the wire byte and not a value the client reconstructed.
        wire_flags: u8,
    },
    Cdf53 {
        pass_idx: u8,
        #[serde(with = "serde_bytes")]
        bit_planes: Vec<u8>,
        /// `Some` only on pass 0. The GPU decoder needs it to tell a
        /// skipped-because-empty trailing plane from an undelivered one.
        present_passes: Option<u16>,
    },
}

/// Expand a `Cdf53Wire` pass's streams into 384 plane bytes, or `None`.
pub(crate) fn expand_wire_planes(streams: &[u8]) -> Option<Vec<u8>> {
    let mut planes = vec![0u8; 384];
    let dst: &mut [u8; 384] = planes.as_mut_slice().try_into().ok()?;
    ghostframe_client_core::cdf53_prevalidate::decode_cdf53_wire(streams, dst).ok()?;
    Some(planes)
}

impl WasmEvent {
    /// [`From<Event>`], except that a `Cdf53Wire` pass is expanded to the
    /// `Cdf53` mirror, and dropped (`None`) if it will not expand, like the
    /// native renderer does.
    pub fn from_event(ev: Event) -> Option<WasmEvent> {
        match ev {
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data:
                    TileData::Cdf53Wire {
                        pass_idx,
                        streams,
                        present_passes,
                        new_generation: _,
                    },
            } => Some(WasmEvent::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data: WasmTileData::Cdf53 {
                    pass_idx,
                    bit_planes: expand_wire_planes(&streams)?,
                    present_passes,
                },
            }),
            other @ (Event::TileReady { .. }
            | Event::TilePayload { .. }
            | Event::PaletteUpdated { .. }
            | Event::FrameDimensions { .. }
            | Event::NeedsH264 { .. }
            | Event::DecodeError { .. }
            | Event::Evicted { .. }) => Some(other.into()),
        }
    }
}

impl From<TileData> for WasmTileData {
    /// By value: the payload `Vec`s move into the mirror instead of being
    /// cloned. Every tile event used to copy its payload here and then again
    /// inside serde-wasm-bindgen (PC-3); the second copy is the one the
    /// JS `Uint8Array` needs, the first was pure overhead.
    fn from(data: TileData) -> Self {
        match data {
            TileData::Solid(quad) => WasmTileData::Solid {
                bytes: quad.to_vec(),
            },
            TileData::PalRle {
                palette_id,
                count,
                indices,
                wire_flags,
            } => WasmTileData::PalRle {
                palette_id,
                count,
                indices,
                wire_flags,
            },
            // `new_generation` is not forwarded yet: the browser's GPU
            // accumulator keys on the generation number alone, as the CPU
            // one did before it had this flag (see `TileData::Cdf53`).
            TileData::Cdf53 {
                pass_idx,
                bit_planes,
                present_passes,
                new_generation: _,
            } => WasmTileData::Cdf53 {
                pass_idx,
                bit_planes,
                present_passes,
            },
            // Callers go through `WasmEvent::from_event`, which expands (or
            // drops) wire passes first; the wasm core never sets
            // `ClientConfig::cdf53_wire` anyway. This arm only completes the
            // match.
            TileData::Cdf53Wire {
                pass_idx,
                streams,
                present_passes,
                new_generation: _,
            } => {
                debug_assert!(false, "Cdf53Wire reached the plain From conversion");
                WasmTileData::Cdf53 {
                    pass_idx,
                    bit_planes: expand_wire_planes(&streams).unwrap_or_default(),
                    present_passes,
                }
            }
        }
    }
}

impl From<&TileData> for WasmTileData {
    fn from(data: &TileData) -> Self {
        data.clone().into()
    }
}

impl From<Event> for WasmEvent {
    fn from(ev: Event) -> Self {
        match ev {
            Event::TileReady {
                frame_seq,
                tile_x,
                tile_y,
                rgba,
            } => WasmEvent::TileReady {
                frame_seq,
                tile_x,
                tile_y,
                rgba,
            },
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data,
            } => WasmEvent::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data: data.into(),
            },
            Event::PaletteUpdated { palette_id, colors } => {
                WasmEvent::PaletteUpdated { palette_id, colors }
            }
            Event::FrameDimensions { width, height } => {
                WasmEvent::FrameDimensions { width, height }
            }
            Event::NeedsH264 {
                frame_seq,
                timestamp_us,
                is_keyframe,
                ref_frame_seq,
                flags,
                slots,
                reset_decoder,
                payload,
            } => WasmEvent::NeedsH264 {
                frame_seq,
                timestamp_us,
                is_keyframe,
                ref_frame_seq,
                flags,
                slots,
                reset_decoder,
                payload,
            },
            Event::DecodeError {
                codec,
                tile_x,
                tile_y,
                code,
            } => WasmEvent::DecodeError {
                codec: codec as u8,
                tile_x,
                tile_y,
                code: code as u8,
            },
            Event::Evicted { reason } => WasmEvent::Evicted {
                reason: reason.to_byte(),
            },
        }
    }
}

impl From<&Event> for WasmEvent {
    fn from(ev: &Event) -> Self {
        ev.clone().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_client_core::{DecodeErrorCode, Event, TileData};
    use ghostframe_protocol::protocol::Codec;

    #[test]
    fn a_wire_pass_is_expanded_or_dropped_never_empty() {
        let wire = |streams: Vec<u8>| Event::TilePayload {
            frame_seq: 1,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Cdf53Wire {
                pass_idx: 3,
                streams,
                present_passes: None,
                new_generation: false,
            },
        };
        let mut good = Vec::new();
        for _ in 0..3 {
            good.extend_from_slice(&[0, 1, 0xFF]);
        }
        match WasmEvent::from_event(wire(good)) {
            Some(WasmEvent::TilePayload {
                data: WasmTileData::Cdf53 { bit_planes, .. },
                ..
            }) => assert_eq!(bit_planes, vec![0u8; 384]),
            other => panic!("{other:?}"),
        }
        assert!(WasmEvent::from_event(wire(vec![0, 1, 0xFE])).is_none());
    }

    #[test]
    fn tile_payload_mirrors_every_field() {
        let ev = Event::TilePayload {
            frame_seq: 0x1234_5678,
            tile_x: 3,
            tile_y: 7,
            generation: 2,
            data: TileData::Cdf53 {
                pass_idx: 13,
                bit_planes: vec![1, 2, 3],
                present_passes: None,
                new_generation: false,
            },
        };
        match WasmEvent::from(&ev) {
            WasmEvent::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data,
            } => {
                assert_eq!(frame_seq, 0x1234_5678);
                assert_eq!(tile_x, 3);
                assert_eq!(tile_y, 7);
                assert_eq!(generation, 2);
                match data {
                    WasmTileData::Cdf53 {
                        pass_idx,
                        bit_planes,
                        present_passes: _,
                    } => {
                        assert_eq!(pass_idx, 13);
                        assert_eq!(bit_planes, vec![1, 2, 3]);
                    }
                    other => panic!("wrong TileData variant: {other:?}"),
                }
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

/// Protocol datagram counts, in the exact shape `window.__ghostframeStats`
/// already had.
///
/// `camelCase` deliberately: the e2e tests read `tileDatagrams` /
/// `frameDatagrams` off that global, so matching the existing property names
/// lets `hooks.ts` assign straight across instead of renaming in flight.
///
/// `u32`, not `u64`: `serde_wasm_bindgen` renders a `u64` as a JS `BigInt`,
/// and `BigInt` is not JSON-serializable -- the e2e readers round-trip these
/// through `page.evaluate`, so a `u64` would throw inside the browser instead
/// of returning a number. A session would need over four billion datagrams
/// to reach the saturation point.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WasmDatagramCounts {
    pub tile_datagrams: u32,
    pub frame_datagrams: u32,
}

/// Serde mirror of `Cdf53CoverageSummary`, plus a preformatted `line` so the
/// client does not have to re-derive the same string in TypeScript.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WasmCdf53Coverage {
    pub tiles: u32,
    pub complete: u32,
    pub partial: u32,
    pub bitmap_unknown: u32,
    pub gave_up: u32,
    pub pass_hist: Vec<u32>,
    pub line: String,
}

impl From<&ghostframe_client_core::cdf53_coverage::Cdf53CoverageSummary> for WasmCdf53Coverage {
    fn from(s: &ghostframe_client_core::cdf53_coverage::Cdf53CoverageSummary) -> Self {
        Self {
            tiles: s.tiles,
            complete: s.complete,
            partial: s.partial,
            bitmap_unknown: s.bitmap_unknown,
            gave_up: s.gave_up,
            pass_hist: s.pass_hist.to_vec(),
            line: s.to_log_line(),
        }
    }
}

/// One tile that has not reached its full pass set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WasmCdf53IncompleteTile {
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_mask: u16,
    pub present_passes: u16,
    /// `present_passes & !pass_mask` — the passes still owed.
    pub missing: u16,
    pub sweep_attempts: u8,
}

impl From<(u8, u8, u16, u16, u8)> for WasmCdf53IncompleteTile {
    fn from(
        (tile_x, tile_y, pass_mask, present_passes, sweep_attempts): (u8, u8, u16, u16, u8),
    ) -> Self {
        Self {
            tile_x,
            tile_y,
            pass_mask,
            present_passes,
            missing: present_passes & !pass_mask,
            sweep_attempts,
        }
    }
}
