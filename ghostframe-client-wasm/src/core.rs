// SPDX-License-Identifier: MIT OR Apache-2.0
//! The session boundary. `app::client` drives this.
//!
//! Every export returns a value JS can inspect; none may `unwrap` on
//! wire-derived data. A panic here aborts the whole module — every later
//! call traps — so one malformed datagram would end the session rather than
//! drop a frame.

use std::cell::RefCell;
use std::rc::Rc;

use ghostframe_client_core::{ClientConfig, ClientCore, H264Outcome, TileDelivery};
use wasm_bindgen::prelude::*;

use crate::boundary::{WasmEvent, WasmPollOutput};

/// Serialises `T` for JS, mapping serialisation failure to a `JsValue` error
/// rather than panicking.
fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// The browser's `ClientConfig`. `tile_delivery_payload = true` gives
/// validated, undecoded payloads for the GPU (`TileDelivery::Payload`); false
/// would route tiles down the native Decoded path and hand back RGBA, which
/// looks like it works and is wrong.
pub fn client_config(
    indices_raw_enabled: bool,
    supports_h264: bool,
    tile_delivery_payload: bool,
) -> ClientConfig {
    ClientConfig {
        indices_raw_enabled,
        supports_h264,
        // The browser runs the same reference gate as the native client and
        // sends the v2 feedback (HELLO bit 2): frame acks (0x0A) and reset
        // requests (0x09). H.264 itself is gated by `supports_h264` alone.
        h264_acked_ref: true,
        tile_delivery: if tile_delivery_payload {
            TileDelivery::Payload
        } else {
            TileDelivery::Decoded
        },
        cdf53_wire: false,
        display: None,
        fec_framing: false, // R6-b B5 wires this
    }
}

/// A core for the browser, with this build's version for the stats panel.
pub fn new_client_core(config: ClientConfig, now_us: u64) -> ClientCore {
    let mut core = ClientCore::new(config, now_us);
    core.set_client_version(crate::CLIENT_VERSION);
    core
}

/// Feed a batch of datagrams (`concat` back to back, `lens[i]` the i-th
/// length, `arrivals_us[i]` its arrival) and return the events in arrival
/// order. Malformed batches are processed up to the first inconsistency.
/// The loop `handleDatagrams` always ran (see its doc for the batch format).
pub fn feed_datagrams(
    core: &mut ClientCore,
    concat: &[u8],
    lens: &[u32],
    arrivals_us: &[f64],
) -> Vec<ghostframe_client_core::Event> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    for (len, at) in lens.iter().zip(arrivals_us.iter()) {
        let len = *len as usize;
        let Some(end) = offset.checked_add(len) else {
            break;
        };
        let Some(datagram) = concat.get(offset..end) else {
            break;
        };
        offset = end;
        // `as u64` saturates and maps NaN to 0, so a hostile or buggy
        // caller cannot wrap.
        events.extend(core.handle_datagram(datagram, *at as u64));
    }
    events
}

/// `cdf53Coverage`'s body, shared with `DebugApi`.
pub(crate) fn cdf53_coverage_js(core: &ClientCore) -> Result<JsValue, JsValue> {
    let s = core.cdf53_coverage_summary();
    to_js(&crate::boundary::WasmCdf53Coverage::from(&s))
}

/// `cdf53IncompleteTiles`'s body, shared with `DebugApi`.
pub(crate) fn cdf53_incomplete_js(core: &ClientCore, limit: usize) -> Result<JsValue, JsValue> {
    let v: Vec<crate::boundary::WasmCdf53IncompleteTile> = core
        .cdf53_incomplete_tiles(limit)
        .into_iter()
        .map(Into::into)
        .collect();
    to_js(&v)
}

/// `(tile, frame)` datagrams accepted, saturating into `u32` (a `u64` would
/// cross as a BigInt).
pub(crate) fn datagram_counts(core: &ClientCore) -> (u32, u32) {
    let (tile, frame) = core.datagram_counts();
    (
        u32::try_from(tile).unwrap_or(u32::MAX),
        u32::try_from(frame).unwrap_or(u32::MAX),
    )
}

#[wasm_bindgen]
pub struct WasmClientCore {
    /// Shared with the stats panel (`ghostframe-web-ui`), which reads the
    /// stats and queues subscribe messages from its own timer and click
    /// handlers. Those run as separate browser tasks, never inside one of the
    /// calls below: no method here calls back into JS, so a `borrow_mut` here
    /// cannot find the panel's borrow held.
    inner: Rc<RefCell<ClientCore>>,
}

#[wasm_bindgen]
impl WasmClientCore {
    /// `tile_delivery_payload = true` gives the browser undecoded, validated
    /// payloads for the GPU (`TileDelivery::Payload`). Native consumers pass
    /// `false` and keep `Decoded`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        indices_raw_enabled: bool,
        supports_h264: bool,
        tile_delivery_payload: bool,
        now_us: u64,
        fec_framing: bool,
    ) -> WasmClientCore {
        WasmClientCore {
            inner: Rc::new(RefCell::new(new_client_core(
                ClientConfig {
                    fec_framing,
                    ..client_config(indices_raw_enabled, supports_h264, tile_delivery_payload)
                },
                now_us,
            ))),
        }
    }

    /// Feed bytes read from the feedback stream's return half, in whatever
    /// chunks the transport produced. Carries the stats schema and samples;
    /// malformed input is counted by the core, never thrown.
    #[wasm_bindgen(js_name = handleServerStream)]
    pub fn handle_server_stream(&mut self, bytes: &[u8], now_us: u64) {
        self.inner.borrow_mut().handle_server_stream(bytes, now_us);
    }

    /// Note `n` frames painted, for the panel's painted-fps row. Called by
    /// `app::client`.
    #[wasm_bindgen(js_name = recordPaints)]
    pub fn record_paints(&mut self, n: u32) {
        self.inner.borrow_mut().record_paints(n);
    }

    /// Ask the server for a fresh IDR (feedback stream 0x09): the browser's
    /// decoder failed. `reason` is a `ResetReason` discriminant (1 = decoder
    /// failure); anything else is ignored. The request is queued like any
    /// other outbound stream message and leaves through `pollTransmit`. The
    /// server answers at most one a second; the caller paces itself (2.2 s,
    /// as the native renderer does).
    #[wasm_bindgen(js_name = requestReset)]
    pub fn request_reset(&mut self, reason: u8, frame_seq: u32) {
        if let Some(r) = ghostframe_client_core::ResetReason::from_u8(reason) {
            self.inner.borrow_mut().request_reset(r, frame_seq);
        }
    }

    /// Report what the browser's decoder did with an H.264 access unit that a
    /// `NeedsH264` event handed out. `outcome`: 0 = decoded (the decoder's
    /// output callback fired for this frame's timestamp), 1 = not output
    /// (dropped, or superseded by a later output), 2 = decode error, 3 = no
    /// decoder. Anything else is ignored. The gate learns which frames
    /// became references, the frame ack (0x0A) is queued (at most one per
    /// 4 ms), and an error also queues the 0x09 IDR request (paced 2.2 s);
    /// all of it leaves through `pollTransmit`.
    #[wasm_bindgen(js_name = h264Result)]
    pub fn h264_result(&mut self, frame_seq: u32, outcome: u8, now_us: u64) {
        let outcome = match outcome {
            0 => H264Outcome::Decoded,
            1 => H264Outcome::NotOutput,
            2 => H264Outcome::DecodeError,
            3 => H264Outcome::NoDecoder,
            _ => return,
        };
        self.inner
            .borrow_mut()
            .h264_result(frame_seq, outcome, now_us);
    }

    /// `[total, streak]`: access units the decoder consumed without a picture
    /// for them, and the run since the last one that produced its own. A long
    /// streak on a stream that plays is a decoder with output delay.
    #[wasm_bindgen(js_name = h264NotOutputStats)]
    pub fn h264_not_output_stats(&self) -> Vec<f64> {
        let (total, streak) = self.inner.borrow().h264_not_output_stats();
        vec![total as f64, streak as f64]
    }

    /// Feed one inbound datagram; returns the resulting events as an array.
    ///
    /// Tiles, dimensions, palettes and H.264 access units arrive as
    /// datagrams, through here. The feedback bidi stream's return half
    /// carries the server's stats frames only, through
    /// [`WasmClientCore::handle_server_stream`].
    #[wasm_bindgen(js_name = handleDatagram)]
    pub fn handle_datagram(&mut self, bytes: &[u8], now_us: u64) -> Result<JsValue, JsValue> {
        let events = self.inner.borrow_mut().handle_datagram(bytes, now_us);
        let mirrored: Vec<WasmEvent> = events
            .into_iter()
            .filter_map(WasmEvent::from_event)
            .collect();
        to_js(&mirrored)
    }

    /// Feed a batch of inbound datagrams in one boundary crossing; returns
    /// all resulting events, in arrival order, as one array.
    ///
    /// `concat` holds the datagrams back to back, `lens[i]` is the length of
    /// the i-th, and `arrivals_us[i]` is the time it was *received* (µs on the
    /// same clock as every other `now_us`). Per-datagram arrival stamps are
    /// kept rather than one timestamp for the batch because ACK arrival times,
    /// suspension detection and RTT sampling all read them: stamping a whole
    /// rAF's worth of datagrams with one instant would make the path look
    /// bursty when it is not.
    ///
    /// Times cross as `f64` (exact for any µs value below 2^53, ~285 years),
    /// which spares the BigInt allocation the `u64` form costs per call.
    /// Malformed batches (length arrays that disagree with `concat`) are
    /// processed up to the first inconsistency rather than trapping.
    #[wasm_bindgen(js_name = handleDatagrams)]
    pub fn handle_datagrams(
        &mut self,
        concat: &[u8],
        lens: &[u32],
        arrivals_us: &[f64],
    ) -> Result<JsValue, JsValue> {
        let events = feed_datagrams(&mut self.inner.borrow_mut(), concat, lens, arrivals_us);
        let mirrored: Vec<WasmEvent> = events
            .into_iter()
            .filter_map(WasmEvent::from_event)
            .collect();
        to_js(&mirrored)
    }

    /// Fire due timers (ACK/NACK flush, tail sweep,
    /// periodic feedback); returns the resulting events.
    #[wasm_bindgen(js_name = onTimeout)]
    pub fn on_timeout(&mut self, now_us: u64) -> Result<JsValue, JsValue> {
        let events = self.inner.borrow_mut().on_timeout(now_us);
        let mirrored: Vec<WasmEvent> = events
            .into_iter()
            .filter_map(WasmEvent::from_event)
            .collect();
        to_js(&mirrored)
    }

    /// Drain one pending outbound buffer; returns `undefined` when empty.
    /// Call until it returns `undefined`.
    #[wasm_bindgen(js_name = pollTransmit)]
    pub fn poll_transmit(&mut self, now_us: u64) -> Result<JsValue, JsValue> {
        while let Some(out) = self.inner.borrow_mut().poll_transmit(now_us) {
            if let Some(out) = WasmPollOutput::from_output(out) {
                return to_js(&out);
            }
        }
        Ok(JsValue::UNDEFINED)
    }

    /// Earliest µs deadline at which `onTimeout` must be called.
    ///
    /// Returns `undefined` only if the core has no armed deadline. In
    /// practice the tail-sweep and feedback deadlines are always armed, so
    /// this is always a number — but JS must not assume that.
    #[wasm_bindgen(js_name = pollTimeout)]
    pub fn poll_timeout(&self) -> Option<u64> {
        self.inner.borrow().poll_timeout()
    }

    /// Per-tile Cdf53 coverage, for the client's diagnostics log.
    ///
    /// Returns `{ tiles, complete, partial, bitmap_unknown, gave_up,
    /// pass_hist, line }`, where `line` is a preformatted one-line summary.
    ///
    /// This exists because the browser cannot derive coverage any more: the
    /// TypeScript decode path that maintained `window.__cdf53Coverage` was
    /// deleted at the wasm cutover, and the log line that read it reported
    /// `tiles=0 refined=0` on sessions that had decoded thousands of tiles.
    #[wasm_bindgen(js_name = cdf53Coverage)]
    pub fn cdf53_coverage(&self) -> Result<JsValue, JsValue> {
        cdf53_coverage_js(&self.inner.borrow())
    }

    /// Samples the stats rings have accepted since this core was created.
    /// `u32`, not `u64`: a `u64` crosses as a BigInt, and at one sample a
    /// second this overflows after 136 years. Published by `hooks.ts` (`serverStream`) as
    /// `window.__ghostframeStatsSamples` for the e2e suite.
    #[wasm_bindgen(js_name = statsSamplesAccepted)]
    pub fn stats_samples_accepted(&self) -> u32 {
        self.inner
            .borrow()
            .stats()
            .samples_accepted()
            .min(u64::from(u32::MAX)) as u32
    }

    /// Complete server-stream frames received since this core was created,
    /// counted before any check. Published as `window.__ghostframeStatsFrames`.
    #[wasm_bindgen(js_name = statsFramesReceived)]
    pub fn stats_frames_received(&self) -> u32 {
        self.inner
            .borrow()
            .stats()
            .frames_received()
            .min(u64::from(u32::MAX)) as u32
    }

    /// Protocol datagrams accepted so far, as `{ tileDatagrams,
    /// frameDatagrams }` -- the shape `window.__ghostframeStats` carries.
    ///
    /// The browser cannot derive this any more: the TypeScript receive loop
    /// that incremented those counters went at the wasm cutover, leaving
    /// the old `main.ts` holding a `stats` object it never wrote to. Five e2e tests
    /// read the globals and saw 0 on sessions moving thousands of datagrams.
    /// Classified by `ClientCore::handle_datagram`'s own dispatch rather than
    /// re-tested here, so the two cannot drift.
    #[wasm_bindgen(js_name = datagramCounts)]
    pub fn datagram_counts(&self) -> Result<JsValue, JsValue> {
        let (tile_datagrams, frame_datagrams) = datagram_counts(&self.inner.borrow());
        to_js(&crate::boundary::WasmDatagramCounts {
            tile_datagrams,
            frame_datagrams,
        })
    }

    /// Tiles still missing at least one present pass, most-stalled first.
    ///
    /// `limit` caps the list so a fully stalled screen cannot produce a
    /// 2040-entry log line. Each entry is
    /// `{ tile_x, tile_y, pass_mask, present_passes, missing, sweep_attempts }`.
    #[wasm_bindgen(js_name = cdf53IncompleteTiles)]
    pub fn cdf53_incomplete_tiles(&self, limit: usize) -> Result<JsValue, JsValue> {
        cdf53_incomplete_js(&self.inner.borrow(), limit)
    }

    /// Encode a receiver-feedback report for the bidi stream.
    #[wasm_bindgen(js_name = encodeFeedback)]
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8> {
        self.inner.borrow_mut().encode_feedback(now_us)
    }
}

impl WasmClientCore {
    /// The shared core, for the panel. Not exported to JS.
    pub(crate) fn shared(&self) -> Rc<RefCell<ClientCore>> {
        self.inner.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel and the JS-facing wrapper must be looking at one core: the
    /// version set at construction, a server stream and the paint count all
    /// land in the core `shared()` hands the panel.
    #[test]
    fn the_panel_shares_the_core_the_exports_drive() {
        let mut core = WasmClientCore::new(true, true, true, 0);
        let shared = core.shared();
        assert_eq!(
            shared.borrow().stats().view(0).client_version,
            crate::CLIENT_VERSION
        );
        core.handle_server_stream(&[0x02, 0xff, 0xff], 0); // oversized frame
        assert_eq!(shared.borrow().stats().samples_rejected(), 1);
        shared.borrow_mut().on_timeout(1_000_000);
        core.record_paints(7);
        shared.borrow_mut().on_timeout(2_000_000);
        let view = shared.borrow().stats().view(2_000_000);
        let painted = view
            .groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.label == "Painted frames")
            .and_then(|r| r.current);
        assert_eq!(painted, Some(7.0));
    }
}

/// Core suites that lived in vitest and drove the wasm package through its JS
/// exports. They test `ClientCore` as the Rust client configures it
/// (`new_client_core(client_config(..), 0)`), so the browser config is under
/// test too.
#[cfg(test)]
mod moved_from_vitest {
    use super::*;
    use ghostframe_client_core::{Event, PollOutput};
    use ghostframe_protocol::ack::AckBatch;
    use ghostframe_protocol::eviction::{build_eviction_datagram, EvictionReason};
    use ghostframe_protocol::protocol::{
        build_frame_dimensions_datagram, fragment_tile, Codec, TileFragmentInputs, NO_ACK,
        TILE_DATAGRAM_FLAG,
    };

    fn core_with(indices_raw: bool, h264: bool, payload: bool) -> ClientCore {
        new_client_core(client_config(indices_raw, h264, payload), 0)
    }

    fn tile(
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        codec: Codec,
        pass: u8,
        payload: &[u8],
        max_fragment: usize,
    ) -> Vec<Vec<u8>> {
        fragment_tile(
            &TileFragmentInputs {
                frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
                tile_x,
                tile_y,
                codec,
                generation: 1,
                pass,
                timestamp_us: 0,
            },
            payload,
            max_fragment,
        )
    }

    fn drain(core: &mut ClientCore, now_us: u64) -> Vec<PollOutput> {
        let mut out = Vec::new();
        while let Some(o) = core.poll_transmit(now_us) {
            out.push(o);
        }
        out
    }

    /// One H.264 frame datagram: the 22-byte v2 header and `payload`.
    fn frame_datagram(
        frame_seq: u32,
        frag_idx: u16,
        frag_total: u16,
        wire_seq: u32,
        idr: bool,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&frame_seq.to_be_bytes());
        v.extend_from_slice(&frag_idx.to_be_bytes());
        v.extend_from_slice(&frag_total.to_be_bytes());
        v.extend_from_slice(&wire_seq.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // timestamp_us
        v.push(u8::from(idr)); // flags
        v.push(0); // slots
        v.extend_from_slice(&(if idr { NO_ACK } else { 0 }).to_be_bytes()); // ref_frame_seq
        v.extend_from_slice(payload);
        v
    }

    // ---- datagram_batch.test.ts ----

    /// Five CDF 5/3 tiles of two passes each, reversed.
    fn scene() -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        // Pass 0: present_passes = 0b11, then three all-zero sign planes.
        let pass0 = [0, 0b11, 0, 1, 0xff, 0, 1, 0xff, 0, 1, 0xff];
        for t in 0..5u8 {
            let lit = 1 + t * 13;
            let mut payload = [0u8; 12];
            for ch in 0..3 {
                payload[ch * 4..ch * 4 + 4].copy_from_slice(&[0, 2, lit, 0xfe]);
            }
            out.extend(tile(1 + u32::from(t), t, 2, Codec::Cdf53, 0, &pass0, 1200));
            out.extend(tile(
                1 + u32::from(t),
                t,
                2,
                Codec::Cdf53,
                1,
                &payload,
                1200,
            ));
        }
        out.reverse();
        out
    }

    fn concat(dgs: &[Vec<u8>]) -> (Vec<u8>, Vec<u32>) {
        (dgs.concat(), dgs.iter().map(|d| d.len() as u32).collect())
    }

    /// Replaces datagram_batch.test.ts "yields the same events as
    /// per-datagram handleDatagram".
    #[test]
    fn a_batch_yields_the_events_of_one_datagram_at_a_time() {
        let dgs = scene();
        let mut seq = core_with(false, false, false);
        let mut expected = Vec::new();
        for (i, d) in dgs.iter().enumerate() {
            expected.extend(seq.handle_datagram(d, 1000 + 3000 * i as u64));
        }
        // Non-vacuous: the scene must complete tiles.
        assert!(expected
            .iter()
            .any(|e| matches!(e, Event::TileReady { .. })));
        let expected: Vec<String> = expected.iter().map(|e| format!("{e:?}")).collect();

        let mut batch = core_with(false, false, false);
        let (bytes, lens) = concat(&dgs);
        let arrivals: Vec<f64> = (0..dgs.len()).map(|i| 1000.0 + 3000.0 * i as f64).collect();
        let got = feed_datagrams(&mut batch, &bytes, &lens, &arrivals);
        let got: Vec<String> = got.iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(got, expected);
        assert_eq!(datagram_counts(&batch), datagram_counts(&seq));
        // Arrivals are 3 ms apart, so the ACKs it queued carry different
        // millisecond stamps: one stamp for the whole batch would differ here.
        let a = drain_acks(&mut batch);
        let b = drain_acks(&mut seq);
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }

    /// Every ack entry (wire_seq, arrival_time_ms_lo16) the core has queued
    /// by t = 1 s.
    fn drain_acks(core: &mut ClientCore) -> Vec<(u32, u16)> {
        core.on_timeout(1_000_000);
        let mut v = Vec::new();
        for o in drain(core, 1_000_000) {
            if let PollOutput::Datagram(b) = o {
                if b[0] == 0x06 {
                    let batch = AckBatch::decode(&b).unwrap();
                    v.extend(
                        batch
                            .entries
                            .iter()
                            .map(|e| (e.wire_seq, e.arrival_time_ms_lo16)),
                    );
                }
            }
        }
        v
    }

    /// Replaces datagram_batch.test.ts "tolerates length arrays that
    /// disagree with the buffer".
    #[test]
    fn a_lying_length_array_processes_the_consistent_prefix() {
        let dgs: Vec<Vec<u8>> = scene().into_iter().take(4).collect();
        let (bytes, lens) = concat(&dgs);
        // More length than exists in the last entry: the prefix is processed.
        let mut core = core_with(false, false, false);
        let mut lying = lens.clone();
        *lying.last_mut().unwrap() += 1000;
        feed_datagrams(&mut core, &bytes, &lying, &[0.0; 4]);
        assert_eq!(datagram_counts(&core), (dgs.len() as u32 - 1, 0));
        // Fewer arrival stamps than lengths: only the paired prefix.
        let mut core2 = core_with(false, false, false);
        feed_datagrams(&mut core2, &bytes, &lens, &[0.0; 2]);
        assert_eq!(datagram_counts(&core2), (2, 0));
        // NaN stamps neither wrap nor trap, and the datagrams are still fed.
        let mut core3 = core_with(false, false, false);
        feed_datagrams(&mut core3, &bytes, &lens, &[f64::NAN; 4]);
        assert_eq!(datagram_counts(&core3), (4, 0));
    }

    // ---- eviction.test.ts ----

    /// Replaces eviction.test.ts "surfaces as an Evicted event carrying the
    /// reason code".
    #[test]
    fn the_eviction_notice_is_one_evicted_event_with_its_reason() {
        let mut core = core_with(false, false, false);
        let dg = build_eviction_datagram(EvictionReason::DisplacedByNewSession);
        let events = core.handle_datagram(&dg, 0);
        let evicted: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Evicted { reason } => Some(*reason),
                _ => None,
            })
            .collect();
        assert_eq!(evicted, vec![EvictionReason::DisplacedByNewSession]);
        assert_eq!(evicted[0].to_byte(), 1);
    }

    /// Replaces eviction.test.ts "does not fire on an ordinary tile datagram".
    #[test]
    fn an_ordinary_tile_never_evicts() {
        let mut core = core_with(false, false, false);
        let frags = tile(7, 3, 4, Codec::Solid, 0, &[0; 4], 8);
        assert!(!frags.is_empty());
        for f in frags {
            assert!(!core
                .handle_datagram(&f, 0)
                .iter()
                .any(|e| matches!(e, Event::Evicted { .. })));
        }
    }

    // ---- h264_ack.test.ts ----

    /// The wire_seqs named by every acknowledgement batch queued by `now_us`.
    fn acked_wire_seqs(core: &mut ClientCore, now_us: u64) -> Vec<u32> {
        core.on_timeout(now_us);
        let mut seqs = Vec::new();
        for o in drain(core, now_us) {
            if let PollOutput::Datagram(b) = o {
                if b[0] == 0x06 {
                    seqs.extend(
                        AckBatch::decode(&b)
                            .unwrap()
                            .entries
                            .iter()
                            .map(|e| e.wire_seq),
                    );
                }
            }
        }
        seqs.sort_unstable();
        seqs
    }

    /// Replaces h264_ack.test.ts "are acknowledged by wire_seq, and a NO_ACK
    /// fragment is not".
    #[test]
    fn h264_fragments_are_acknowledged_by_wire_seq_and_no_ack_ones_are_not() {
        let mut core = core_with(true, true, true);
        drain(&mut core, 0); // the HELLO
        let mut events =
            core.handle_datagram(&frame_datagram(7, 0, 2, 41, true, &[1, 2, 3]), 1_000);
        events.extend(core.handle_datagram(&frame_datagram(7, 1, 2, 42, true, &[4, 5]), 2_000));
        // A fragment of another frame the server does not track.
        core.handle_datagram(&frame_datagram(8, 0, 1, NO_ACK, false, &[9]), 3_000);
        // Reassembled, so the datagrams were taken, not dropped.
        assert!(events.iter().any(|e| matches!(e, Event::NeedsH264 { .. })));
        assert_eq!(acked_wire_seqs(&mut core, 20_000), vec![41, 42]);
    }

    /// Replaces h264_ack.test.ts "a fragment older than the window is still
    /// acknowledged".
    #[test]
    fn a_fragment_older_than_the_window_is_still_acknowledged() {
        let mut core = core_with(true, true, true);
        drain(&mut core, 0);
        core.handle_datagram(&frame_datagram(50, 0, 1, 1, false, &[1]), 1_000);
        let stale = core.handle_datagram(&frame_datagram(10, 0, 1, 2, false, &[1]), 2_000);
        assert!(stale.is_empty());
        assert_eq!(acked_wire_seqs(&mut core, 20_000), vec![1, 2]);
    }

    // ---- h264_result.test.ts (core half) ----

    #[derive(Debug, PartialEq)]
    struct FrameAck {
        newest: u32,
        decoded: u64,
        refused: u64,
    }

    struct Drained {
        acks: Vec<FrameAck>,
        resets: Vec<(u8, u32)>,
        order: Vec<u8>,
    }

    fn frame_acks(core: &mut ClientCore, now_us: u64) -> Drained {
        core.on_timeout(now_us);
        let mut d = Drained {
            acks: vec![],
            resets: vec![],
            order: vec![],
        };
        for o in drain(core, now_us) {
            match o {
                PollOutput::Datagram(b) => {
                    d.order.push(b[0]);
                    if b[0] == 0x0a {
                        d.acks.push(FrameAck {
                            newest: u32::from_le_bytes(b[1..5].try_into().unwrap()),
                            decoded: u64::from_le_bytes(b[5..13].try_into().unwrap()),
                            refused: u64::from_le_bytes(b[13..21].try_into().unwrap()),
                        });
                    }
                }
                PollOutput::Stream(b) if b[0] == 0x09 => {
                    d.resets
                        .push((b[1], u32::from_le_bytes(b[2..6].try_into().unwrap())));
                }
                PollOutput::Stream(_) => {}
            }
        }
        d
    }

    fn frame(seq: u32, idr: bool, reference: u32) -> Vec<u8> {
        let mut v = frame_datagram(seq, 0, 1, seq, idr, &[7, 7]);
        if !idr {
            v[18..22].copy_from_slice(&reference.to_be_bytes());
        }
        v
    }

    /// A browser core with its HELLO drained, returned alongside.
    fn h264_core() -> (ClientCore, Vec<PollOutput>) {
        let mut core = core_with(true, true, true);
        let hello = drain(&mut core, 0);
        (core, hello)
    }

    /// Replaces h264_result.test.ts "sets HELLO 0x04 next to 0x02".
    #[test]
    fn the_hello_sets_0x04_next_to_0x02() {
        let (_, hello) = h264_core();
        let stream = hello
            .iter()
            .find_map(|o| match o {
                PollOutput::Stream(b) if b[0] == 0x03 => Some(b),
                _ => None,
            })
            .expect("a HELLO on the stream");
        assert_eq!(stream[1] & 0x04, 0x04);
        assert_eq!(stream[1] & 0x02, 0x02);
    }

    /// Replaces h264_result.test.ts "acks a decoded frame, little-endian, and
    /// a refused one in the refused mask".
    #[test]
    fn acks_a_decoded_frame_and_a_refused_one_in_the_refused_mask() {
        let (mut core, _) = h264_core();
        let ev = core.handle_datagram(&frame(1, true, 0), 1_000);
        assert!(ev.iter().any(|e| matches!(
            e,
            Event::NeedsH264 {
                reset_decoder: true,
                ..
            }
        )));
        assert!(frame_acks(&mut core, 1_000).acks.is_empty()); // fed, not yet decoded
        core.h264_result(1, H264Outcome::Decoded, 2_000);
        // The ack rides the ack batch: nothing due before it, then the batch
        // (0x06) and the frame ack (0x0A) in one drain.
        assert!(frame_acks(&mut core, 2_000).order.is_empty());
        let first = frame_acks(&mut core, 6_000);
        assert_eq!(first.order, vec![0x06, 0x0a]);
        assert_eq!(
            first.acks,
            vec![FrameAck {
                newest: 1,
                decoded: 1,
                refused: 0
            }]
        );
        // Frame 2 never arrives; 3 names it: refused at the gate.
        let ev3 = core.handle_datagram(&frame(3, false, 2), 10_000);
        assert!(!ev3.iter().any(|e| matches!(e, Event::NeedsH264 { .. })));
        let d = frame_acks(&mut core, 16_000);
        assert_eq!(
            d.acks,
            vec![FrameAck {
                newest: 3,
                decoded: 0b100,
                refused: 0b001
            }]
        );
        assert_eq!(d.resets, vec![(4, 3)]); // GateReset, asked at once
    }

    /// Replaces h264_result.test.ts "turns a decode error into one paced
    /// decoder-failure request".
    #[test]
    fn a_decode_error_is_one_paced_decoder_failure_request() {
        let (mut core, _) = h264_core();
        core.handle_datagram(&frame(1, true, 0), 0);
        core.h264_result(1, H264Outcome::Decoded, 0);
        core.handle_datagram(&frame(2, false, 1), 1_000);
        core.h264_result(2, H264Outcome::DecodeError, 1_000);
        assert_eq!(frame_acks(&mut core, 1_000).resets, vec![(1, 2)]);
        core.h264_result(3, H264Outcome::DecodeError, 2_000);
        assert!(frame_acks(&mut core, 2_000).resets.is_empty());
    }

    /// Replaces h264_result.test.ts "counts access units that came out with
    /// no picture of their own".
    #[test]
    fn counts_access_units_with_no_picture_of_their_own() {
        let (mut core, _) = h264_core();
        core.handle_datagram(&frame(1, true, 0), 0);
        core.h264_result(1, H264Outcome::NotOutput, 0);
        core.handle_datagram(&frame(2, false, 1), 1_000);
        core.h264_result(2, H264Outcome::NotOutput, 1_000);
        assert_eq!(core.h264_not_output_stats(), (2, 2));
        core.handle_datagram(&frame(3, false, 2), 2_000);
        core.h264_result(3, H264Outcome::Decoded, 2_000);
        assert_eq!(core.h264_not_output_stats(), (2, 0));
    }

    /// Replaces h264_result.test.ts "ignores an outcome it does not know", at
    /// the export's u8 mapping.
    #[test]
    fn h264_result_ignores_an_unknown_outcome() {
        // The same session twice; only the outcome byte differs. A known one
        // (0 = decoded) must yield a frame ack, so the unknown one's silence
        // is the mapping's doing and not an unreachable precondition.
        let acks_after = |outcome: u8| {
            let mut wasm = WasmClientCore::new(true, true, true, 0);
            let shared = wasm.shared();
            drain(&mut shared.borrow_mut(), 0);
            shared.borrow_mut().handle_datagram(&frame(1, true, 0), 0);
            wasm.h264_result(1, outcome, 0);
            let d = frame_acks(&mut shared.borrow_mut(), 20_000);
            d.acks
        };
        assert!(!acks_after(0).is_empty());
        assert!(acks_after(99).is_empty());
    }

    // ---- reset_request.test.ts ----

    /// Replaces reset_request.test.ts "queues a 0x09 decoder-failure request
    /// on the stream".
    #[test]
    fn request_reset_queues_a_0x09_decoder_failure() {
        let mut wasm = WasmClientCore::new(true, true, true, 0);
        let shared = wasm.shared();
        drain(&mut shared.borrow_mut(), 0);
        wasm.request_reset(1, 0x0102_0304);
        let out = shared.borrow_mut().poll_transmit(0);
        match out {
            Some(PollOutput::Stream(b)) => {
                assert_eq!(b[0], 0x09);
                assert_eq!(b[1], 1); // ResetReason::DecoderFailure
                assert_eq!(&b[2..6], &[4, 3, 2, 1]); // little-endian
            }
            other => panic!("expected a stream message, got {other:?}"),
        }
    }

    /// Replaces reset_request.test.ts "ignores a reason it does not know".
    #[test]
    fn request_reset_ignores_an_unknown_reason() {
        let mut wasm = WasmClientCore::new(true, true, true, 0);
        let shared = wasm.shared();
        drain(&mut shared.borrow_mut(), 0);
        wasm.request_reset(200, 1);
        assert!(shared.borrow_mut().poll_transmit(0).is_none());
    }

    // ---- tile_assembly.test.ts ----

    /// A valid CDF 5/3 pass body: three channels of `[len=2][literal][zero
    /// run]`, each with its own literal.
    fn channels(lits: [u8; 3]) -> Vec<u8> {
        let mut p = vec![0u8; 12];
        for (ch, lit) in lits.iter().enumerate() {
            p[ch * 4..ch * 4 + 4].copy_from_slice(&[0, 2, *lit, 0xfe]);
        }
        p
    }

    /// The 384 bit-plane bytes the core hands up for `channels(lits)`.
    fn planes(lits: [u8; 3]) -> Vec<u8> {
        let mut out = vec![0u8; 384];
        for (ch, lit) in lits.iter().enumerate() {
            out[ch * 128] = *lit;
        }
        out
    }

    /// `(frame_seq, tile_x, tile_y, pass_idx, bit_planes)` of each completed
    /// tile in the events.
    fn payload_events(core: &mut ClientCore, dg: &[u8]) -> Vec<(u32, u8, u8, u8, Vec<u8>)> {
        core.handle_datagram(dg, 0)
            .into_iter()
            .filter_map(|e| match e {
                Event::TilePayload {
                    frame_seq,
                    tile_x,
                    tile_y,
                    data:
                        ghostframe_client_core::TileData::Cdf53 {
                            pass_idx,
                            bit_planes,
                            ..
                        },
                    ..
                } => Some((frame_seq, tile_x, tile_y, pass_idx, bit_planes)),
                _ => None,
            })
            .collect()
    }

    /// Replaces tile_assembly.test.ts "a tile pass completes from its single
    /// datagram".
    #[test]
    fn a_tile_pass_completes_from_its_single_datagram() {
        let mut core = core_with(false, false, true);
        let lits = [0x05, 0x06, 0x07];
        let frags = tile(7, 3, 4, Codec::Cdf53, 9, &channels(lits), 1200);
        assert_eq!(frags.len(), 1);
        assert_eq!(
            payload_events(&mut core, &frags[0]),
            vec![(7, 3, 4, 9, planes(lits))]
        );
    }

    /// Replaces tile_assembly.test.ts "a tile split over several datagrams is
    /// refused, whatever order it arrives in".
    #[test]
    fn a_tile_split_over_several_datagrams_is_refused() {
        let mut core = core_with(false, false, true);
        let frags = tile(7, 3, 4, Codec::Cdf53, 9, &channels([1, 2, 3]), 8);
        assert_eq!(frags.len(), 2);
        for f in frags.iter().rev() {
            assert!(payload_events(&mut core, f).is_empty());
        }
    }

    /// Replaces tile_assembly.test.ts "passes, frames and tiles keep their
    /// own identity in the event".
    #[test]
    fn passes_frames_and_tiles_keep_their_own_identity_in_the_event() {
        let mut core = core_with(false, false, true);
        let cases: [(u32, u8, u8, u8, [u8; 3]); 4] = [
            (0x1234, 10, 30, 0, [0x01, 0x02, 0x03]),
            (0x1234, 10, 30, 1, [0x40, 0x41, 0x42]),
            (100, 2, 3, 4, [0x11, 0x12, 0x13]),
            (101, 2, 3, 4, [0x44, 0x45, 0x46]),
        ];
        for (seq, tx, ty, pass, lits) in cases {
            let body = if pass == 0 {
                [vec![0x00, 0x03], channels(lits)].concat()
            } else {
                channels(lits)
            };
            let frags = tile(seq, tx, ty, Codec::Cdf53, pass, &body, 1200);
            assert_eq!(
                payload_events(&mut core, &frags[0]),
                vec![(seq, tx, ty, pass, planes(lits))]
            );
        }
    }

    /// Frame dimensions still arrive as one event (the builder the
    /// TypeScript suites could not call).
    #[test]
    fn frame_dimensions_arrive_as_an_event() {
        let mut core = core_with(false, false, true);
        let dg = build_frame_dimensions_datagram(1, 0, 640, 480);
        let ev = core.handle_datagram(&dg, 0);
        assert!(ev.iter().any(|e| matches!(
            e,
            Event::FrameDimensions {
                width: 640,
                height: 480
            }
        )));
    }
}
