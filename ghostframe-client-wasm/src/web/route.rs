// SPDX-License-Identifier: MIT OR Apache-2.0
//! Where the core's events go in the browser: tile work into the renderer
//! (inside wasm, so tile bytes never cross into JS), the rest to JS.
//!
//! JS still keeps the e2e bookkeeping (`__ghostframeRecordedTiles`, the
//! `__h5_tilePushLog`, ...), which needs a tile's codec, coordinates and size
//! but not its bytes. Each tile therefore comes back as a *slim* record: the
//! full payload length and at most [`SAMPLE_LEN`] leading bytes (enough for
//! the hex prefix `cdf53_globals.ts` logs and for `solid-red` marking).

use ghostframe_client_core::{Event, TileData};
use serde::Serialize;

use crate::boundary::{expand_wire_planes, WasmEvent};
use crate::web::limits::check_frame_dimensions;

/// Leading payload bytes a slim record carries.
pub const SAMPLE_LEN: usize = 16;

/// What applies tile events: the renderer, or a fake in the tests.
pub trait TileSink {
    /// Apply one event (tile payload, palette update or frame dimensions).
    fn apply(&mut self, ev: Event) -> Result<(), String>;
}

/// A tile payload as JS sees it: field names match `WasmTileData`, the byte
/// fields hold at most [`SAMPLE_LEN`] bytes, and `len` is the real length.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "codec")]
pub enum SlimTileData {
    Solid {
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
        len: usize,
    },
    PalRle {
        palette_id: u8,
        count: u8,
        wire_flags: u8,
        #[serde(with = "serde_bytes")]
        indices: Vec<u8>,
        len: usize,
    },
    Cdf53 {
        pass_idx: u8,
        #[serde(with = "serde_bytes")]
        bit_planes: Vec<u8>,
        present_passes: Option<u16>,
        len: usize,
    },
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind")]
pub enum SlimEvent {
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        generation: u8,
        data: SlimTileData,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    PaletteUpdated {
        palette_id: u8,
        colors: Vec<[u8; 4]>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    FrameDimensions {
        width: u32,
        height: u32,
        applied: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Cannot happen (the core delivers payloads); reported, never rendered.
    TileReady {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
    },
}

/// One entry of the array JS gets back.
#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Routed {
    Tile(SlimEvent),
    /// `NeedsH264`, `DecodeError`, `Evicted`: exactly what `handleDatagrams`
    /// returned before (the H.264 payload has to reach the JS decoder).
    Passthrough(WasmEvent),
}

fn sample(bytes: &[u8]) -> Vec<u8> {
    bytes[..bytes.len().min(SAMPLE_LEN)].to_vec()
}

fn slim_of(data: &TileData) -> Option<SlimTileData> {
    Some(match data {
        TileData::Solid(bgra) => SlimTileData::Solid {
            bytes: bgra.to_vec(),
            len: bgra.len(),
        },
        TileData::PalRle {
            palette_id,
            count,
            indices,
            wire_flags,
        } => SlimTileData::PalRle {
            palette_id: *palette_id,
            count: *count,
            wire_flags: *wire_flags,
            indices: sample(indices),
            len: indices.len(),
        },
        TileData::Cdf53 {
            pass_idx,
            bit_planes,
            present_passes,
            new_generation: _,
        } => SlimTileData::Cdf53 {
            pass_idx: *pass_idx,
            bit_planes: sample(bit_planes),
            present_passes: *present_passes,
            len: bit_planes.len(),
        },
        // Expanded to `Cdf53` before this is called.
        TileData::Cdf53Wire { .. } => return None,
    })
}

/// Apply the tile work in `events` to `sink`, in order, and return the records
/// JS needs, in the same order. Never stops early: a failing event is carried
/// in its own record.
pub fn route<S: TileSink + ?Sized>(events: Vec<Event>, sink: &mut S) -> Vec<Routed> {
    let mut out = Vec::with_capacity(events.len());
    for ev in events {
        match ev {
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data,
            } => {
                let data = if let TileData::Cdf53Wire {
                    pass_idx,
                    streams,
                    present_passes,
                    new_generation,
                } = data
                {
                    // Only produced with `cdf53_wire` on, which the browser never
                    // sets; expanded (or dropped) as the native renderer does.
                    let Some(bit_planes) = expand_wire_planes(&streams) else {
                        continue;
                    };
                    TileData::Cdf53 {
                        pass_idx,
                        bit_planes,
                        present_passes,
                        new_generation,
                    }
                } else {
                    data
                };
                let Some(slim) = slim_of(&data) else {
                    continue;
                };
                let error = sink
                    .apply(Event::TilePayload {
                        frame_seq,
                        tile_x,
                        tile_y,
                        generation,
                        data,
                    })
                    .err();
                out.push(Routed::Tile(SlimEvent::TilePayload {
                    frame_seq,
                    tile_x,
                    tile_y,
                    generation,
                    data: slim,
                    error,
                }));
            }
            Event::PaletteUpdated { palette_id, colors } => {
                let error = sink
                    .apply(Event::PaletteUpdated {
                        palette_id,
                        colors: colors.clone(),
                    })
                    .err();
                out.push(Routed::Tile(SlimEvent::PaletteUpdated {
                    palette_id,
                    colors,
                    error,
                }));
            }
            Event::FrameDimensions { width, height } => {
                let verdict = check_frame_dimensions(width, height)
                    .and_then(|()| sink.apply(Event::FrameDimensions { width, height }));
                out.push(Routed::Tile(SlimEvent::FrameDimensions {
                    width,
                    height,
                    applied: verdict.is_ok(),
                    reason: verdict.err(),
                }));
            }
            Event::TileReady {
                frame_seq,
                tile_x,
                tile_y,
                rgba: _,
            } => {
                out.push(Routed::Tile(SlimEvent::TileReady {
                    frame_seq,
                    tile_x,
                    tile_y,
                }));
            }
            ev @ (Event::NeedsH264 { .. } | Event::DecodeError { .. } | Event::Evicted { .. }) => {
                if let Some(w) = WasmEvent::from_event(ev) {
                    out.push(Routed::Passthrough(w));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what it was given; fails the tiles it is told to.
    #[derive(Default)]
    struct FakeSink {
        seen: Vec<Event>,
        fail_tile: Option<(u8, u8)>,
        fail_resize: bool,
    }

    impl TileSink for FakeSink {
        fn apply(&mut self, ev: Event) -> Result<(), String> {
            let fails = match &ev {
                Event::TilePayload { tile_x, tile_y, .. } => {
                    self.fail_tile == Some((*tile_x, *tile_y))
                }
                Event::FrameDimensions { .. } => self.fail_resize,
                _ => false,
            };
            self.seen.push(ev);
            if fails {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
    }

    fn palrle(frame_seq: u32, tx: u8, ty: u8) -> Event {
        Event::TilePayload {
            frame_seq,
            tile_x: tx,
            tile_y: ty,
            generation: 0,
            data: TileData::PalRle {
                palette_id: 3,
                count: 2,
                indices: (0..512u32).map(|i| i as u8).collect(),
                wire_flags: 0b11,
            },
        }
    }

    #[test]
    fn a_tile_reaches_the_sink_whole_and_js_gets_a_sample_and_the_full_length() {
        let mut sink = FakeSink::default();
        let out = route(vec![palrle(9, 1, 2)], &mut sink);
        match &sink.seen[0] {
            Event::TilePayload {
                data: TileData::PalRle { indices, .. },
                ..
            } => {
                assert_eq!(indices.len(), 512, "the sink must get every byte");
            }
            other => panic!("{other:?}"),
        }
        match &out[..] {
            [Routed::Tile(SlimEvent::TilePayload {
                frame_seq: 9,
                tile_x: 1,
                tile_y: 2,
                data:
                    SlimTileData::PalRle {
                        palette_id: 3,
                        count: 2,
                        wire_flags: 0b11,
                        indices,
                        len,
                    },
                error: None,
                ..
            })] => {
                assert_eq!(*len, 512);
                assert_eq!(indices.len(), SAMPLE_LEN);
                assert_eq!(indices[..4], [0, 1, 2, 3]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn solid_is_carried_whole_and_cdf53_is_sampled_with_its_length() {
        let mut sink = FakeSink::default();
        let solid = Event::TilePayload {
            frame_seq: 1,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([1, 2, 3, 4]),
        };
        let cdf = Event::TilePayload {
            frame_seq: 1,
            tile_x: 1,
            tile_y: 0,
            generation: 2,
            data: TileData::Cdf53 {
                pass_idx: 4,
                bit_planes: vec![7; 384],
                present_passes: Some(0x3FFF),
                new_generation: false,
            },
        };
        let out = route(vec![solid, cdf], &mut sink);
        assert!(matches!(
            &out[0],
            Routed::Tile(SlimEvent::TilePayload { data: SlimTileData::Solid { bytes, len: 4 }, .. }) if bytes == &[1, 2, 3, 4]
        ));
        assert!(matches!(
            &out[1],
            Routed::Tile(SlimEvent::TilePayload {
                data: SlimTileData::Cdf53 { pass_idx: 4, bit_planes, present_passes: Some(0x3FFF), len: 384 }, ..
            }) if bit_planes.len() == SAMPLE_LEN
        ));
    }

    #[test]
    fn order_is_kept_and_one_failing_tile_does_not_stop_the_batch() {
        let mut sink = FakeSink {
            fail_tile: Some((1, 1)),
            ..Default::default()
        };
        let out = route(
            vec![palrle(1, 0, 0), palrle(1, 1, 1), palrle(1, 2, 2)],
            &mut sink,
        );
        assert_eq!(
            sink.seen.len(),
            3,
            "the tile after the failing one was still applied"
        );
        let errors: Vec<bool> = out
            .iter()
            .map(|r| {
                matches!(
                    r,
                    Routed::Tile(SlimEvent::TilePayload { error: Some(_), .. })
                )
            })
            .collect();
        assert_eq!(errors, [false, true, false]);
    }

    #[test]
    fn a_bad_frame_size_is_reported_and_not_applied_and_a_good_one_is() {
        let mut sink = FakeSink::default();
        let out = route(
            vec![
                Event::FrameDimensions {
                    width: 0,
                    height: 480,
                },
                Event::FrameDimensions {
                    width: 64,
                    height: 64,
                },
            ],
            &mut sink,
        );
        assert_eq!(
            sink.seen.len(),
            1,
            "only the valid size reaches the renderer"
        );
        assert!(matches!(
            &out[0],
            Routed::Tile(SlimEvent::FrameDimensions {
                applied: false,
                reason: Some(_),
                ..
            })
        ));
        assert!(matches!(
            &out[1],
            Routed::Tile(SlimEvent::FrameDimensions {
                width: 64,
                height: 64,
                applied: true,
                reason: None
            })
        ));
    }

    #[test]
    fn a_resize_the_renderer_refuses_is_reported_with_its_reason() {
        let mut sink = FakeSink {
            fail_resize: true,
            ..Default::default()
        };
        let out = route(
            vec![Event::FrameDimensions {
                width: 64,
                height: 64,
            }],
            &mut sink,
        );
        assert!(matches!(
            &out[0],
            Routed::Tile(SlimEvent::FrameDimensions { applied: false, reason: Some(r), .. }) if r == "boom"
        ));
    }

    #[test]
    fn palette_updates_are_applied_and_reported_with_their_colours() {
        let mut sink = FakeSink::default();
        let out = route(
            vec![Event::PaletteUpdated {
                palette_id: 7,
                colors: vec![[1, 2, 3, 4], [5, 6, 7, 8]],
            }],
            &mut sink,
        );
        assert!(
            matches!(&sink.seen[0], Event::PaletteUpdated { palette_id: 7, colors } if colors.len() == 2)
        );
        assert!(matches!(
            &out[0],
            Routed::Tile(SlimEvent::PaletteUpdated { palette_id: 7, colors, error: None }) if colors[1] == [5, 6, 7, 8]
        ));
    }

    #[test]
    fn h264_decode_errors_and_evictions_go_to_js_untouched() {
        let mut sink = FakeSink::default();
        let h264 = Event::NeedsH264 {
            frame_seq: 5,
            timestamp_us: 1,
            is_keyframe: true,
            ref_frame_seq: 0xFFFF_FFFF,
            flags: 0,
            slots: 0,
            reset_decoder: true,
            payload: vec![9; 10],
        };
        let out = route(vec![h264], &mut sink);
        assert!(
            sink.seen.is_empty(),
            "H.264 is the escape's job, not the tile renderer's"
        );
        assert!(
            matches!(&out[0], Routed::Passthrough(WasmEvent::NeedsH264 { payload, .. }) if payload.len() == 10)
        );

        let mut sink = FakeSink::default();
        let out = route(
            vec![
                Event::DecodeError {
                    codec: ghostframe_protocol::protocol::Codec::PalRle,
                    tile_x: 1,
                    tile_y: 2,
                    code: ghostframe_client_core::DecodeErrorCode::PayloadTooShort,
                },
                Event::Evicted {
                    reason: ghostframe_protocol::eviction::EvictionReason::DisplacedByNewSession,
                },
            ],
            &mut sink,
        );
        assert!(sink.seen.is_empty(), "neither is the tile renderer's");
        assert_eq!(out.len(), 2);
        assert!(matches!(
            &out[0],
            Routed::Passthrough(WasmEvent::DecodeError { .. })
        ));
        assert!(matches!(
            &out[1],
            Routed::Passthrough(WasmEvent::Evicted { .. })
        ));
    }

    #[test]
    fn a_tile_ready_is_reported_and_not_applied() {
        // The core is built with tile_delivery_payload = true, so this cannot
        // happen; the client (`app::session`) logs it as an error rather than render it.
        let mut sink = FakeSink::default();
        let out = route(
            vec![Event::TileReady {
                frame_seq: 1,
                tile_x: 0,
                tile_y: 0,
                rgba: vec![0; 4096],
            }],
            &mut sink,
        );
        assert!(sink.seen.is_empty());
        assert!(matches!(
            &out[0],
            Routed::Tile(SlimEvent::TileReady { frame_seq: 1, .. })
        ));
    }

    #[test]
    fn an_unexpandable_cdf53_wire_pass_is_dropped_like_the_native_renderer_does() {
        let mut sink = FakeSink::default();
        let wire = Event::TilePayload {
            frame_seq: 1,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Cdf53Wire {
                pass_idx: 0,
                streams: vec![1, 2, 3],
                present_passes: None,
                new_generation: false,
            },
        };
        let out = route(vec![wire], &mut sink);
        assert!(out.is_empty());
        assert!(sink.seen.is_empty());
    }

    #[test]
    fn the_serialised_shape_is_what_main_ts_reads() {
        let mut sink = FakeSink::default();
        let out = route(
            vec![
                palrle(9, 1, 2),
                Event::FrameDimensions {
                    width: 64,
                    height: 64,
                },
                Event::NeedsH264 {
                    frame_seq: 5,
                    timestamp_us: 1,
                    is_keyframe: false,
                    ref_frame_seq: 4,
                    flags: 0,
                    slots: 0,
                    reset_decoder: false,
                    payload: vec![1, 2, 3],
                },
            ],
            &mut sink,
        );
        let v = serde_json::to_value(&out).expect("serialises");
        assert_eq!(v[0]["kind"], "TilePayload");
        assert_eq!(v[0]["frame_seq"], 9);
        assert_eq!(v[0]["data"]["codec"], "PalRle");
        assert_eq!(v[0]["data"]["len"], 512);
        assert_eq!(v[0]["data"]["wire_flags"], 3);
        assert_eq!(v[0]["data"]["indices"].as_array().map(Vec::len), Some(16));
        assert!(
            v[0].get("error").is_none(),
            "no error key unless there is one"
        );
        assert_eq!(v[1]["kind"], "FrameDimensions");
        assert_eq!(v[1]["applied"], true);
        assert_eq!(v[2]["kind"], "NeedsH264");
        assert_eq!(v[2]["payload"].as_array().map(Vec::len), Some(3));
    }
}
