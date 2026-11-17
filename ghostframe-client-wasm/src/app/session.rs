// SPDX-License-Identifier: MIT OR Apache-2.0
//! What the page does with each routed event (port of `main.ts`'s
//! `handleEvent`, minus the e2e recorders, which `hooks.ts` keeps until PR 4).

use ghostframe_protocol::protocol::TILE_NACK_ENVELOPE;

use crate::app::close::evicted_message;
use crate::boundary::WasmEvent;
use crate::web::route::{Routed, SlimEvent, SlimTileData};

/// Events applied since the last tick, per codec, for the page log's `Δdrain`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Drained {
    pub solid: u32,
    pub palrle: u32,
    pub cdf53: u32,
    pub h264: u32,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Status(&'static str),
    Log(String),
    ConsoleError(String),
    Overlay(&'static str),
    /// Hand this access unit to the H.264 decoder.
    DecodeH264 {
        frame_seq: u32,
        is_keyframe: bool,
        payload: Vec<u8>,
    },
}

#[derive(Debug, Default)]
pub struct Session {
    first_frame_seen: bool,
    drained: Drained,
}

fn hex_spaced(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Session {
    /// One routed event; its actions go to `out`. The record comes back when
    /// the hooks want it (tiles, palettes, frame sizes, decode errors).
    /// `NeedsH264` (its payload is taken for the decoder), `Evicted` and
    /// `TileReady` do not: no recorder reads them.
    pub fn on_routed(&mut self, r: Routed, out: &mut Vec<Action>) -> Option<Routed> {
        match r {
            Routed::Tile(SlimEvent::TilePayload {
                tile_x,
                tile_y,
                ref data,
                ..
            }) => {
                let (sample, len) = match data {
                    SlimTileData::Solid { bytes, len } => {
                        self.drained.solid += 1;
                        (bytes, *len)
                    }
                    SlimTileData::PalRle { indices, len, .. } => {
                        self.drained.palrle += 1;
                        (indices, *len)
                    }
                    SlimTileData::Cdf53 {
                        bit_planes, len, ..
                    } => {
                        self.drained.cdf53 += 1;
                        (bit_planes, *len)
                    }
                };
                if !self.first_frame_seen {
                    self.first_frame_seen = true;
                    out.push(Action::Log(format!(
                        "First tile: ({tile_x},{tile_y}) {len}B"
                    )));
                    out.push(Action::Log(format!("First bytes: {}", hex_spaced(sample))));
                    out.push(Action::Status("Receiving frames"));
                }
                Some(r)
            }
            Routed::Tile(SlimEvent::FrameDimensions {
                applied: false,
                ref reason,
                ..
            }) => {
                let msg = format!(
                    "FrameDimensions rejected: {}",
                    reason.as_deref().unwrap_or("")
                );
                out.push(Action::ConsoleError(msg.clone()));
                out.push(Action::Log(msg));
                Some(r)
            }
            Routed::Tile(SlimEvent::FrameDimensions { .. })
            | Routed::Tile(SlimEvent::PaletteUpdated { .. })
            | Routed::Passthrough(WasmEvent::DecodeError { .. }) => Some(r),
            Routed::Tile(SlimEvent::TileReady {
                frame_seq,
                tile_x,
                tile_y,
            }) => {
                out.push(Action::ConsoleError(format!(
                    "Unexpected TileReady event (seq {frame_seq}, tile {tile_x},{tile_y}): the core \
                     was constructed with tile_delivery_payload=true, so Payload-mode reassembly \
                     should never emit this. No rendering path exists for it."
                )));
                None
            }
            Routed::Passthrough(WasmEvent::NeedsH264 {
                frame_seq,
                is_keyframe,
                payload,
                ..
            }) => {
                self.drained.h264 += 1;
                if !self.first_frame_seen {
                    self.first_frame_seen = true;
                    let key = if is_keyframe { "(keyframe)" } else { "" };
                    out.push(Action::Log(format!(
                        "First full frame: {}B {key}",
                        payload.len()
                    )));
                    out.push(Action::Status("Receiving frames"));
                }
                out.push(Action::DecodeH264 {
                    frame_seq,
                    is_keyframe,
                    payload,
                });
                None
            }
            Routed::Passthrough(WasmEvent::Evicted { reason }) => {
                // Terminal: the server has dropped this session. No reconnect:
                // two clients that retry evict each other indefinitely.
                out.push(Action::Overlay(evicted_message(reason)));
                None
            }
            // `handle_datagram` never returns these through `route` (it maps
            // only NeedsH264, DecodeError and Evicted to Passthrough).
            Routed::Passthrough(other) => {
                out.push(Action::ConsoleError(format!(
                    "unexpected routed event: {other:?}"
                )));
                None
            }
        }
    }

    /// The drain counts since the last call.
    pub fn take_drained(&mut self) -> Drained {
        std::mem::take(&mut self.drained)
    }
}

/// NACK entries in an outbound datagram (a leading `TILE_NACK_ENVELOPE` and a
/// count byte), for the page log's `nack_sent`.
pub fn nacks_in(datagram: &[u8]) -> u32 {
    match datagram {
        [TILE_NACK_ENVELOPE, n, ..] => u32::from(*n),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(x: u8, y: u8, bytes: Vec<u8>) -> Routed {
        let len = bytes.len();
        Routed::Tile(SlimEvent::TilePayload {
            frame_seq: 1,
            tile_x: x,
            tile_y: y,
            generation: 0,
            data: SlimTileData::Solid { bytes, len },
            error: None,
        })
    }

    fn h264(payload: Vec<u8>, is_keyframe: bool) -> Routed {
        Routed::Passthrough(WasmEvent::NeedsH264 {
            frame_seq: 7,
            timestamp_us: 0,
            is_keyframe,
            ref_frame_seq: 0xFFFF_FFFF,
            flags: 0,
            slots: 0,
            reset_decoder: true,
            payload,
        })
    }

    #[test]
    fn the_first_tile_logs_and_says_receiving_frames_once() {
        let mut s = Session::default();
        let mut out = Vec::new();
        let back = s.on_routed(solid(3, 4, vec![0, 0, 0xff, 0xff]), &mut out);
        assert!(back.is_some());
        assert_eq!(
            out,
            vec![
                Action::Log("First tile: (3,4) 4B".into()),
                Action::Log("First bytes: 00 00 ff ff".into()),
                Action::Status("Receiving frames"),
            ]
        );
        out.clear();
        assert!(s
            .on_routed(solid(1, 1, vec![1, 2, 3, 4]), &mut out)
            .is_some());
        assert!(out.is_empty());
    }

    #[test]
    fn the_first_h264_frame_also_counts_as_receiving() {
        let mut s = Session::default();
        let mut out = Vec::new();
        let back = s.on_routed(h264(vec![1, 2, 3, 4, 5], true), &mut out);
        assert!(back.is_none());
        assert_eq!(
            out,
            vec![
                Action::Log("First full frame: 5B (keyframe)".into()),
                Action::Status("Receiving frames"),
                Action::DecodeH264 {
                    frame_seq: 7,
                    is_keyframe: true,
                    payload: vec![1, 2, 3, 4, 5]
                },
            ]
        );
        // A later tile is no longer the first frame.
        out.clear();
        s.on_routed(solid(0, 0, vec![0; 4]), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_rejected_size_is_logged_and_still_recorded() {
        let mut s = Session::default();
        let mut out = Vec::new();
        let r = Routed::Tile(SlimEvent::FrameDimensions {
            width: 9,
            height: 9,
            applied: false,
            reason: Some("too big".into()),
        });
        assert!(s.on_routed(r, &mut out).is_some());
        assert_eq!(
            out,
            vec![
                Action::ConsoleError("FrameDimensions rejected: too big".into()),
                Action::Log("FrameDimensions rejected: too big".into()),
            ]
        );
        out.clear();
        let ok = Routed::Tile(SlimEvent::FrameDimensions {
            width: 9,
            height: 9,
            applied: true,
            reason: None,
        });
        assert!(s.on_routed(ok, &mut out).is_some());
        assert!(out.is_empty());
    }

    #[test]
    fn eviction_shows_the_overlay_and_is_not_recorded() {
        let mut s = Session::default();
        let mut out = Vec::new();
        let back = s.on_routed(
            Routed::Passthrough(WasmEvent::Evicted { reason: 1 }),
            &mut out,
        );
        assert!(back.is_none());
        assert_eq!(out, vec![Action::Overlay(evicted_message(1))]);
        assert_ne!(evicted_message(1), evicted_message(0xEE));
    }

    #[test]
    fn tile_ready_is_an_error_not_a_render() {
        let mut s = Session::default();
        let mut out = Vec::new();
        let r = Routed::Tile(SlimEvent::TileReady {
            frame_seq: 5,
            tile_x: 1,
            tile_y: 2,
        });
        assert!(s.on_routed(r, &mut out).is_none());
        assert_eq!(out.len(), 1);
        match &out[0] {
            Action::ConsoleError(m) => assert!(m.contains("seq 5, tile 1,2"), "{m}"),
            other => panic!("expected a console error, got {other:?}"),
        }
    }

    #[test]
    fn drain_counts_per_codec_reset_on_take() {
        let mut s = Session::default();
        let mut out = Vec::new();
        s.on_routed(solid(0, 0, vec![0; 4]), &mut out);
        s.on_routed(solid(1, 0, vec![0; 4]), &mut out);
        s.on_routed(
            Routed::Tile(SlimEvent::TilePayload {
                frame_seq: 1,
                tile_x: 0,
                tile_y: 0,
                generation: 0,
                data: SlimTileData::PalRle {
                    palette_id: 0,
                    count: 1,
                    wire_flags: 0,
                    indices: vec![0],
                    len: 1,
                },
                error: None,
            }),
            &mut out,
        );
        s.on_routed(
            Routed::Tile(SlimEvent::TilePayload {
                frame_seq: 1,
                tile_x: 0,
                tile_y: 0,
                generation: 0,
                data: SlimTileData::PalRle {
                    palette_id: 0,
                    count: 1,
                    wire_flags: 0,
                    indices: vec![0],
                    len: 1,
                },
                error: None,
            }),
            &mut out,
        );
        s.on_routed(
            Routed::Tile(SlimEvent::TilePayload {
                frame_seq: 1,
                tile_x: 0,
                tile_y: 0,
                generation: 0,
                data: SlimTileData::Cdf53 {
                    pass_idx: 0,
                    bit_planes: vec![0],
                    present_passes: None,
                    len: 1,
                },
                error: None,
            }),
            &mut out,
        );
        s.on_routed(
            Routed::Tile(SlimEvent::TilePayload {
                frame_seq: 1,
                tile_x: 0,
                tile_y: 0,
                generation: 0,
                data: SlimTileData::Cdf53 {
                    pass_idx: 0,
                    bit_planes: vec![0],
                    present_passes: None,
                    len: 1,
                },
                error: None,
            }),
            &mut out,
        );
        s.on_routed(
            Routed::Tile(SlimEvent::TilePayload {
                frame_seq: 1,
                tile_x: 0,
                tile_y: 0,
                generation: 0,
                data: SlimTileData::Cdf53 {
                    pass_idx: 0,
                    bit_planes: vec![0],
                    present_passes: None,
                    len: 1,
                },
                error: None,
            }),
            &mut out,
        );
        s.on_routed(h264(vec![1], false), &mut out);
        assert_eq!(
            s.take_drained(),
            Drained {
                solid: 2,
                palrle: 2,
                cdf53: 3,
                h264: 1
            }
        );
        assert_eq!(s.take_drained(), Drained::default());
    }

    #[test]
    fn nacks_are_counted_from_the_envelope() {
        assert_eq!(nacks_in(&[TILE_NACK_ENVELOPE, 3, 9, 9]), 3);
        let other = TILE_NACK_ENVELOPE.wrapping_add(1);
        assert_eq!(nacks_in(&[other, 3]), 0);
        assert_eq!(nacks_in(&[TILE_NACK_ENVELOPE]), 0);
        assert_eq!(nacks_in(&[]), 0);
    }
}
