// SPDX-License-Identifier: MIT OR Apache-2.0
//! The e2e bookkeeping (port of `hooks.ts`, `cdf53_globals.ts` and the
//! recorders of `diagnostics.ts`). Pure: `apply` turns one `HookCall` into
//! counter updates here and `LogEntry`s for the browser side to append to the
//! `window` arrays (`debug_hooks.rs`). Only built under `?e2e`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ghostframe_client_core::DecodeErrorCode;
use ghostframe_protocol::codec::pal_rle::palette_fingerprint_bgra;
use ghostframe_protocol::protocol::Codec;
use serde::Serialize;

use crate::app::health::Health;
use crate::app::hooks::HookCall;
use crate::boundary::WasmEvent;
use crate::web::route::{Routed, SlimEvent, SlimTileData};

/// Frames whose first-receive stamp is kept (the TS bound).
const MAX_PENDING_FRAME_STAMPS: usize = 1024;
/// Leading bytes of a payload in the `__h5_tilePushLog` hex prefix.
const HEX_PREFIX_LEN: usize = 8;
/// Characters of a renderer error kept in its `gpu:` tally key.
const GPU_TALLY_CHARS: usize = 80;

/// `window.__ghostframeRecordedTiles` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TileRecord {
    pub seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub codec: u8,
    pub payload_len: usize,
    pub fb_width: u32,
    pub fb_height: u32,
    pub first_recv_ms_client: f64,
    pub last_paint_ms_client: f64,
}

/// `window.__ghostframeRecordedResizes` entry; `trigger` is always `"sentinel"`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeRecord {
    pub seq: u32,
    pub old_w: u32,
    pub old_h: u32,
    pub new_w: u32,
    pub new_h: u32,
    pub trigger: &'static str,
}

/// `window.__ghostframe_framePaints` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FramePaint {
    pub seq: u32,
    pub raf_ms_client: f64,
}

/// `window.__h2_clientPaletteWrites` entry: `fp` is the 8-hex-digit
/// `palette_fingerprint_bgra`, `c0` the first colour's 4 bytes as hex.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaletteWrite {
    pub id: u8,
    pub len: usize,
    pub fp: String,
    pub c0: String,
    pub ts: f64,
}

/// `window.__h5_tilePushLog` entry: `seq` is -1 for a decode error, `c0` the
/// first 8 payload bytes as hex (`LEN=0` when empty) or `ERR:<code>`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TilePush {
    pub seq: i64,
    pub tx: u8,
    pub ty: u8,
    pub codec: u8,
    pub c0: String,
    pub ts: f64,
}

/// One append to one `window` log.
#[derive(Debug, Clone, PartialEq)]
pub enum LogEntry {
    Tile(TileRecord),
    Codec(u8),
    Flag(u8),
    Resize(ResizeRecord),
    FramePaint(FramePaint),
    PaletteWrite(PaletteWrite),
    TilePush(TilePush),
}

/// What the hooks keep, and what the getters read.
#[derive(Debug, Default)]
pub struct Recorder {
    pub health: Health,
    pub raf_ticks: u32,
    pub tile_datagrams: u32,
    pub frame_datagrams: u32,
    pub stats_samples: u32,
    pub stats_frames: u32,
    pub cdf53_dispatch_seen: u32,
    pub cdf53_pushed_to_queue: u32,
    /// Read by the stats line only (not published: no e2e reader).
    pub cdf53_prevalidate_fails: u32,
    pub cdf53_last_fail_code: Option<u8>,
    /// The framebuffer size the server last announced (applied).
    fb: (u32, u32),
    first_recv_ms: BTreeMap<u32, f64>,
    first_recv_order: VecDeque<u32>,
    latest_tile_frame_seq: u32,
    pending_frame_paint: BTreeSet<u32>,
    /// `true` under `?e2e`: the solid-colour tallies (main.ts gated them).
    pub diagnostics: bool,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `cdf53_globals.ts`'s `hexPrefix`: up to 8 bytes as hex, `LEN=0` if none.
fn hex_prefix(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return format!("LEN={}", bytes.len());
    }
    hex(&bytes[..bytes.len().min(HEX_PREFIX_LEN)])
}

fn is_cdf53_error(code: u8) -> bool {
    code == DecodeErrorCode::Cdf53BadPass as u8
        || code == DecodeErrorCode::Cdf53Truncated as u8
        || code == DecodeErrorCode::Cdf53RleLength as u8
}

impl Recorder {
    pub fn new(diagnostics: bool) -> Self {
        Self {
            diagnostics,
            ..Self::default()
        }
    }

    /// Run one hook call. Log appends go to `out`, in the order the TS made them.
    pub fn apply(&mut self, call: HookCall, out: &mut Vec<LogEntry>) {
        match call {
            HookCall::Events { at_ms, records } => {
                for r in records {
                    self.record(r, at_ms, out);
                }
            }
            HookCall::DatagramCounts { tile, frame } => {
                self.tile_datagrams = tile;
                self.frame_datagrams = frame;
            }
            HookCall::RafTick(n) => self.raf_ticks = n,
            HookCall::BeforePaint { at_ms } => {
                for seq in std::mem::take(&mut self.pending_frame_paint) {
                    out.push(LogEntry::FramePaint(FramePaint {
                        seq,
                        raf_ms_client: at_ms,
                    }));
                    self.first_recv_ms.remove(&seq);
                    self.first_recv_order.retain(|s| *s != seq);
                }
            }
            // The stats line handles it (Task 4).
            HookCall::AfterPaint { .. } => {}
            HookCall::Swallowed { site, message } => self.health.record(&site, &message),
            HookCall::RendererError { tick, message } => {
                self.health
                    .record("renderer", &format!("tick {tick}: {message}"));
                let head: String = message.chars().take(GPU_TALLY_CHARS).collect();
                self.health.tally(&format!("gpu:{head}"));
            }
            HookCall::ServerStream { samples, frames } => {
                self.stats_samples = samples;
                self.stats_frames = frames;
            }
        }
    }

    fn record(&mut self, r: Routed, at_ms: f64, out: &mut Vec<LogEntry>) {
        match r {
            Routed::Tile(SlimEvent::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                data,
                error,
                ..
            }) => {
                // `recordProtocolEvent`.
                let (codec, sample, len) = match &data {
                    SlimTileData::Solid { bytes, len } => (Codec::Solid, bytes, *len),
                    SlimTileData::PalRle { indices, len, .. } => (Codec::PalRle, indices, *len),
                    SlimTileData::Cdf53 {
                        bit_planes, len, ..
                    } => (Codec::Cdf53, bit_planes, *len),
                };
                out.push(LogEntry::TilePush(TilePush {
                    seq: i64::from(frame_seq),
                    tx: tile_x,
                    ty: tile_y,
                    codec: codec as u8,
                    c0: hex_prefix(sample),
                    ts: at_ms,
                }));
                if codec == Codec::Cdf53 {
                    self.cdf53_dispatch_seen = self.cdf53_dispatch_seen.saturating_add(1);
                    self.cdf53_pushed_to_queue = self.cdf53_pushed_to_queue.saturating_add(1);
                }
                // `handleRecord`.
                if let Some(e) = error {
                    // The renderer refused it; the core has already acknowledged the tile.
                    self.health.record(
                        "renderer.apply",
                        &format!("tile ({tile_x},{tile_y}) seq {frame_seq}: {e}"),
                    );
                }
                if let SlimTileData::Solid { bytes, .. } = &data {
                    if bytes.get(2).is_some_and(|b| *b > 0x80) {
                        self.health.mark(
                            u32::from(tile_x),
                            u32::from(tile_y),
                            u64::from(frame_seq),
                            "solid-red",
                        );
                    }
                    if self.diagnostics {
                        self.health.tally(&format!("solid:{}", hex(bytes)));
                    }
                }
                let first_recv = self.note_frame_tile_received(frame_seq, at_ms);
                out.push(LogEntry::Codec(codec as u8));
                // The wire flags byte, not the palette id: `e2e_indices_raw_handshake`
                // tests its bit 1.
                if let SlimTileData::PalRle { wire_flags, .. } = &data {
                    out.push(LogEntry::Flag(*wire_flags));
                }
                out.push(LogEntry::Tile(TileRecord {
                    seq: frame_seq,
                    tile_x,
                    tile_y,
                    codec: codec as u8,
                    payload_len: len,
                    fb_width: self.fb.0,
                    fb_height: self.fb.1,
                    first_recv_ms_client: first_recv,
                    last_paint_ms_client: at_ms,
                }));
            }
            Routed::Tile(SlimEvent::PaletteUpdated {
                palette_id,
                colors,
                error,
            }) => {
                if let Some(e) = error {
                    self.health
                        .record("renderer.palette", &format!("palette {palette_id}: {e}"));
                }
                let bgra: Vec<u8> = colors.iter().flatten().copied().collect();
                out.push(LogEntry::PaletteWrite(PaletteWrite {
                    id: palette_id,
                    len: bgra.len(),
                    fp: format!("{:08x}", palette_fingerprint_bgra(&bgra)),
                    c0: bgra.get(..4).map(hex).unwrap_or_default(),
                    ts: at_ms,
                }));
            }
            Routed::Tile(SlimEvent::FrameDimensions {
                width,
                height,
                applied: true,
                ..
            }) => {
                // `seq` is diagnostic-only, never asserted on; 0 is a placeholder.
                out.push(LogEntry::Resize(ResizeRecord {
                    seq: 0,
                    old_w: self.fb.0,
                    old_h: self.fb.1,
                    new_w: width,
                    new_h: height,
                    trigger: "sentinel",
                }));
                self.fb = (width, height);
            }
            Routed::Passthrough(WasmEvent::DecodeError {
                codec,
                tile_x,
                tile_y,
                code,
            }) => {
                // No frame_seq crosses the boundary for this event: sentinel it.
                out.push(LogEntry::TilePush(TilePush {
                    seq: -1,
                    tx: tile_x,
                    ty: tile_y,
                    codec,
                    c0: format!("ERR:{code}"),
                    ts: at_ms,
                }));
                if codec == Codec::Cdf53 as u8 {
                    self.cdf53_dispatch_seen = self.cdf53_dispatch_seen.saturating_add(1);
                    if is_cdf53_error(code) {
                        self.cdf53_prevalidate_fails =
                            self.cdf53_prevalidate_fails.saturating_add(1);
                        self.cdf53_last_fail_code = Some(code);
                    }
                }
            }
            // A rejected size, `TileReady`, `NeedsH264` and `Evicted` have no recorder.
            Routed::Tile(SlimEvent::FrameDimensions { .. } | SlimEvent::TileReady { .. })
            | Routed::Passthrough(_) => {}
        }
    }

    /// A frame is "done" once the server has moved two sequences past it (the
    /// core's stale-assembly threshold); it then waits in `pending_frame_paint`
    /// for the next `BeforePaint`. Returns the frame's first-receive stamp.
    fn note_frame_tile_received(&mut self, frame_seq: u32, now_ms: f64) -> f64 {
        // A tile for a frame already two sequences behind was (or will never be)
        // recorded; re-creating its entry would emit a duplicate paint record.
        if frame_seq.saturating_add(2) <= self.latest_tile_frame_seq {
            return now_ms;
        }
        if let std::collections::btree_map::Entry::Vacant(e) = self.first_recv_ms.entry(frame_seq) {
            e.insert(now_ms);
            self.first_recv_order.push_back(frame_seq);
            // rAF (the only drain) stops in a hidden tab while datagrams keep
            // arriving: bound the map.
            if self.first_recv_ms.len() > MAX_PENDING_FRAME_STAMPS {
                if let Some(oldest) = self.first_recv_order.pop_front() {
                    self.first_recv_ms.remove(&oldest);
                    self.pending_frame_paint.remove(&oldest);
                }
            }
        }
        if frame_seq > self.latest_tile_frame_seq {
            self.latest_tile_frame_seq = frame_seq;
            let latest = self.latest_tile_frame_seq;
            for seq in self.first_recv_ms.keys() {
                if seq.saturating_add(2) <= latest {
                    self.pending_frame_paint.insert(*seq);
                }
            }
        }
        self.first_recv_ms
            .get(&frame_seq)
            .copied()
            .unwrap_or(now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(seq: u32, x: u8, y: u8, data: SlimTileData) -> Routed {
        Routed::Tile(SlimEvent::TilePayload {
            frame_seq: seq,
            tile_x: x,
            tile_y: y,
            generation: 0,
            data,
            error: None,
        })
    }

    fn solid(seq: u32, bytes: [u8; 4]) -> Routed {
        tile(
            seq,
            0,
            0,
            SlimTileData::Solid {
                bytes: bytes.to_vec(),
                len: 4,
            },
        )
    }

    fn cdf53(seq: u32, plane: Vec<u8>) -> Routed {
        let len = plane.len();
        tile(
            seq,
            1,
            2,
            SlimTileData::Cdf53 {
                pass_idx: 0,
                bit_planes: plane,
                present_passes: None,
                len,
            },
        )
    }

    fn events(r: &mut Recorder, at_ms: f64, records: Vec<Routed>) -> Vec<LogEntry> {
        let mut out = Vec::new();
        r.apply(HookCall::Events { at_ms, records }, &mut out);
        out
    }

    fn decode_error(codec: u8, code: u8) -> Routed {
        Routed::Passthrough(WasmEvent::DecodeError {
            codec,
            tile_x: 3,
            tile_y: 4,
            code,
        })
    }

    fn pushes(out: &[LogEntry]) -> Vec<&TilePush> {
        out.iter()
            .filter_map(|e| match e {
                LogEntry::TilePush(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    /// Source: `cdf53_globals.test.ts`.
    #[test]
    fn the_five_protocol_counters_move_on_a_valid_and_a_malformed_cdf53_tile() {
        let mut r = Recorder::new(false);
        let mut out = events(&mut r, 1.0, vec![cdf53(7, vec![0xab, 0xcd])]);
        assert_eq!(r.cdf53_dispatch_seen, 1);
        assert_eq!(r.cdf53_pushed_to_queue, 1);
        assert_eq!(r.cdf53_prevalidate_fails, 0);
        out.extend(events(
            &mut r,
            2.0,
            vec![decode_error(
                Codec::Cdf53 as u8,
                DecodeErrorCode::Cdf53Truncated as u8,
            )],
        ));
        assert_eq!(r.cdf53_dispatch_seen, 2);
        assert_eq!(r.cdf53_pushed_to_queue, 1);
        assert_eq!(r.cdf53_prevalidate_fails, 1);
        assert_eq!(
            r.cdf53_last_fail_code,
            Some(DecodeErrorCode::Cdf53Truncated as u8)
        );
        let p = pushes(&out);
        assert_eq!(p.len(), 2);
        assert_eq!(
            (p[0].seq, p[0].codec, p[0].c0.as_str()),
            (7, Codec::Cdf53 as u8, "abcd")
        );
        assert_eq!(p[1].seq, -1);
        assert!(p[1].c0.starts_with("ERR:"));
    }

    /// Source: `cdf53_globals.test.ts` (the codec guard on a decode error).
    #[test]
    fn a_non_cdf53_decode_error_does_not_count_as_a_cdf53_dispatch() {
        let mut r = Recorder::new(false);
        let out = events(
            &mut r,
            1.0,
            vec![
                decode_error(Codec::PalRle as u8, DecodeErrorCode::IndexOob as u8),
                // A Cdf53 codec with a non-Cdf53 code still counts as a dispatch only.
                decode_error(Codec::Cdf53 as u8, DecodeErrorCode::IndexOob as u8),
            ],
        );
        assert_eq!(r.cdf53_dispatch_seen, 1);
        assert_eq!(r.cdf53_prevalidate_fails, 0);
        assert_eq!(r.cdf53_last_fail_code, None);
        assert_eq!(pushes(&out).len(), 2);
    }

    /// Source: `diagnostics.ts` `recordTile` and `hooks.ts` `handleRecord`.
    #[test]
    fn a_tile_records_codec_flag_and_tile_in_order() {
        let mut r = Recorder::new(false);
        let mut sink = Vec::new();
        r.apply(
            HookCall::Events {
                at_ms: 1.0,
                records: vec![Routed::Tile(SlimEvent::FrameDimensions {
                    width: 640,
                    height: 480,
                    applied: true,
                    reason: None,
                })],
            },
            &mut sink,
        );
        let pal = tile(
            5,
            1,
            1,
            SlimTileData::PalRle {
                palette_id: 3,
                count: 2,
                wire_flags: 0x02,
                indices: vec![9; 40],
                len: 40,
            },
        );
        let out = events(&mut r, 12.5, vec![pal]);
        assert_eq!(out.len(), 4, "{out:?}");
        assert!(matches!(out[0], LogEntry::TilePush(_)));
        assert_eq!(out[1], LogEntry::Codec(Codec::PalRle as u8));
        assert_eq!(out[2], LogEntry::Flag(0x02));
        match &out[3] {
            LogEntry::Tile(t) => {
                assert_eq!((t.fb_width, t.fb_height), (640, 480));
                assert_eq!((t.payload_len, t.seq), (40, 5));
                assert_eq!(
                    (t.first_recv_ms_client, t.last_paint_ms_client),
                    (12.5, 12.5)
                );
            }
            other => panic!("not a tile: {other:?}"),
        }
        let out = events(&mut r, 13.0, vec![solid(6, [1, 2, 3, 255])]);
        assert_eq!(out.len(), 3);
        assert!(!out.iter().any(|e| matches!(e, LogEntry::Flag(_))));
    }

    /// Source: `diagnostics.test.ts` "recordTile works without the optional
    /// fields". In Rust both timestamps are always present (no `undefined`):
    /// a tile of a frame already two sequences behind has no first-receive
    /// stamp, and still records both as numbers, the first receive falling
    /// back to the arrival time.
    #[test]
    fn a_stale_tile_still_records_both_timestamps() {
        let mut r = Recorder::new(false);
        events(&mut r, 1.0, vec![solid(10, [0; 4])]);
        let out = events(&mut r, 2.0, vec![solid(7, [0; 4])]);
        match out.last() {
            Some(LogEntry::Tile(t)) => {
                assert_eq!(t.seq, 7);
                assert_eq!((t.first_recv_ms_client, t.last_paint_ms_client), (2.0, 2.0));
            }
            other => panic!("not a tile: {other:?}"),
        }
    }

    /// Source: `hooks.ts` `FrameDimensions` arm.
    #[test]
    fn a_rejected_resize_is_not_recorded_and_an_applied_one_is() {
        let mut r = Recorder::new(false);
        let dims = |w, h, applied| {
            Routed::Tile(SlimEvent::FrameDimensions {
                width: w,
                height: h,
                applied,
                reason: None,
            })
        };
        let out = events(
            &mut r,
            1.0,
            vec![dims(800, 600, true), dims(9999, 9999, false)],
        );
        assert_eq!(out.len(), 1);
        let out2 = events(&mut r, 2.0, vec![dims(1024, 768, true)]);
        assert_eq!(
            out2,
            vec![LogEntry::Resize(ResizeRecord {
                seq: 0,
                old_w: 800,
                old_h: 600,
                new_w: 1024,
                new_h: 768,
                trigger: "sentinel",
            })]
        );
        let out3 = events(&mut r, 3.0, vec![solid(1, [0; 4])]);
        match out3.last() {
            Some(LogEntry::Tile(t)) => assert_eq!((t.fb_width, t.fb_height), (1024, 768)),
            other => panic!("{other:?}"),
        }
    }

    /// Source: `diagnostics.test` "recordFramePainted".
    #[test]
    fn frame_paints_are_recorded_two_sequences_later_at_the_paint_time() {
        let mut r = Recorder::new(false);
        events(&mut r, 10.0, vec![solid(1, [0; 4])]);
        events(&mut r, 20.0, vec![solid(3, [0; 4])]);
        let mut out = Vec::new();
        r.apply(HookCall::BeforePaint { at_ms: 30.0 }, &mut out);
        assert_eq!(
            out,
            vec![LogEntry::FramePaint(FramePaint {
                seq: 1,
                raf_ms_client: 30.0
            })]
        );
        // Recorded once only, and its stamp is gone.
        let mut again = Vec::new();
        r.apply(HookCall::BeforePaint { at_ms: 40.0 }, &mut again);
        assert!(again.is_empty());
        assert!(!r.first_recv_ms.contains_key(&1));
        // A late tile of the painted frame does not make a duplicate record.
        events(&mut r, 41.0, vec![solid(1, [0; 4])]);
        // ... not even once a newer frame makes every stamped frame re-scanned.
        events(&mut r, 42.0, vec![solid(4, [0; 4])]);
        r.apply(HookCall::BeforePaint { at_ms: 50.0 }, &mut again);
        assert!(again.is_empty(), "{again:?}");
    }

    /// Source: `hooks.ts` `MAX_PENDING_FRAME_STAMPS`.
    #[test]
    fn pending_frame_stamps_are_bounded() {
        let mut r = Recorder::new(false);
        let mut max_seen = 0;
        for seq in 1..=2000u32 {
            events(&mut r, f64::from(seq), vec![solid(seq, [0; 4])]);
            max_seen = max_seen.max(r.first_recv_ms.len());
            assert!(r.first_recv_ms.len() <= MAX_PENDING_FRAME_STAMPS);
            assert_eq!(r.first_recv_order.len(), r.first_recv_ms.len());
            assert!(r
                .pending_frame_paint
                .iter()
                .all(|s| r.first_recv_ms.contains_key(s)));
        }
        // The bound was reached, so the checks above were not vacuous.
        assert_eq!(max_seen, MAX_PENDING_FRAME_STAMPS);
    }

    /// Source: `diagnostics.ts` `recordPaletteWrite`.
    #[test]
    fn palette_writes_carry_the_server_fingerprint() {
        let mut r = Recorder::new(false);
        let colors = vec![[1u8, 2, 3, 4], [0xaa, 0xbb, 0xcc, 0xdd]];
        let bytes = [1u8, 2, 3, 4, 0xaa, 0xbb, 0xcc, 0xdd];
        let out = events(
            &mut r,
            9.0,
            vec![Routed::Tile(SlimEvent::PaletteUpdated {
                palette_id: 4,
                colors,
                error: Some("nope".into()),
            })],
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            LogEntry::PaletteWrite(w) => {
                assert_eq!(w.id, 4);
                assert_eq!(w.len, 8);
                assert_eq!(w.fp, format!("{:08x}", palette_fingerprint_bgra(&bytes)));
                assert_eq!(w.c0, "01020304");
                assert_eq!(w.ts, 9.0);
            }
            other => panic!("{other:?}"),
        }
        assert!(r
            .health
            .snapshot()
            .first
            .starts_with("renderer.palette: palette 4: nope"));
    }

    /// Source: `hooks.ts` `rendererError` and `swallowed`.
    #[test]
    fn renderer_errors_and_swallowed_errors_reach_health() {
        let mut r = Recorder::new(false);
        let mut out = Vec::new();
        r.apply(
            HookCall::Swallowed {
                site: "x".into(),
                message: "boom".into(),
            },
            &mut out,
        );
        // 100 two-byte chars: truncation is by chars, never in the middle of one.
        let long = "é".repeat(100);
        r.apply(
            HookCall::RendererError {
                tick: 7,
                message: long.clone(),
            },
            &mut out,
        );
        let s = r.health.snapshot();
        assert_eq!(s.swallowed, 2);
        assert_eq!(s.recent[0], "x: boom");
        assert_eq!(s.recent[1], format!("renderer: tick 7: {long}"));
        let key = format!("gpu:{}", "é".repeat(80));
        assert_eq!(s.tallies.get(&key), Some(&1), "{:?}", s.tallies);
        assert!(out.is_empty());
    }

    /// Source: `hooks.ts` `handleRecord` Solid arm (`diagnostics` gate).
    #[test]
    fn solid_tallies_only_under_diagnostics() {
        let red = [0x10, 0x20, 0xff, 0xff];
        let mut off = Recorder::new(false);
        events(&mut off, 1.0, vec![solid(1, red)]);
        let s = off.health.snapshot();
        assert!(s.tallies.is_empty());
        // The red mark is not gated.
        assert_eq!(s.marks.len(), 1);
        assert_eq!(s.marks[0].what, "solid-red");
        let mut on = Recorder::new(true);
        events(&mut on, 1.0, vec![solid(1, red)]);
        assert_eq!(on.health.snapshot().tallies.get("solid:1020ffff"), Some(&1));
    }

    #[test]
    fn counters_are_assigned_from_their_calls() {
        let mut r = Recorder::new(false);
        let mut out = Vec::new();
        r.apply(HookCall::DatagramCounts { tile: 5, frame: 6 }, &mut out);
        r.apply(HookCall::RafTick(9), &mut out);
        r.apply(
            HookCall::ServerStream {
                samples: 2,
                frames: 3,
            },
            &mut out,
        );
        assert_eq!(
            (r.tile_datagrams, r.frame_datagrams, r.raf_ticks),
            (5, 6, 9)
        );
        assert_eq!((r.stats_samples, r.stats_frames), (2, 3));
    }
}
