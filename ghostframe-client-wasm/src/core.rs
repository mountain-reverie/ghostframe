// SPDX-License-Identifier: MIT OR Apache-2.0
//! The session boundary. `main.ts` drives this at cutover.
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
    ) -> WasmClientCore {
        let config = ClientConfig {
            indices_raw_enabled,
            supports_h264,
            // The browser runs the same reference gate as the native client
            // and sends the v2 feedback (HELLO bit 2): frame acks (0x0A) and
            // reset requests (0x09). H.264 itself is gated by
            // `supports_h264` alone.
            h264_acked_ref: true,
            tile_delivery: if tile_delivery_payload {
                TileDelivery::Payload
            } else {
                TileDelivery::Decoded
            },
            cdf53_wire: false,
            display: None,
        };
        let mut core = ClientCore::new(config, now_us);
        core.set_client_version(crate::CLIENT_VERSION);
        WasmClientCore {
            inner: Rc::new(RefCell::new(core)),
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
    /// `main.ts` until the renderer moves into Rust.
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
        let mut core = self.inner.borrow_mut();
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
        drop(core);
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
        match self.inner.borrow_mut().poll_transmit(now_us) {
            Some(out) => to_js(&WasmPollOutput::from(out)),
            None => Ok(JsValue::UNDEFINED),
        }
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
        let s = self.inner.borrow().cdf53_coverage_summary();
        to_js(&crate::boundary::WasmCdf53Coverage::from(&s))
    }

    /// Samples the stats rings have accepted since this core was created.
    /// `u32`, not `u64`: a `u64` crosses as a BigInt, and at one sample a
    /// second this overflows after 136 years. Published by `main.ts` as
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
    /// `main.ts` holding a `stats` object it never wrote to. Five e2e tests
    /// read the globals and saw 0 on sessions moving thousands of datagrams.
    /// Classified by `ClientCore::handle_datagram`'s own dispatch rather than
    /// re-tested here, so the two cannot drift.
    #[wasm_bindgen(js_name = datagramCounts)]
    pub fn datagram_counts(&self) -> Result<JsValue, JsValue> {
        let (tile, frame) = self.inner.borrow().datagram_counts();
        to_js(&crate::boundary::WasmDatagramCounts {
            tile_datagrams: u32::try_from(tile).unwrap_or(u32::MAX),
            frame_datagrams: u32::try_from(frame).unwrap_or(u32::MAX),
        })
    }

    /// Tiles still missing at least one present pass, most-stalled first.
    ///
    /// `limit` caps the list so a fully stalled screen cannot produce a
    /// 2040-entry log line. Each entry is
    /// `{ tile_x, tile_y, pass_mask, present_passes, missing, sweep_attempts }`.
    #[wasm_bindgen(js_name = cdf53IncompleteTiles)]
    pub fn cdf53_incomplete_tiles(&self, limit: usize) -> Result<JsValue, JsValue> {
        let v: Vec<crate::boundary::WasmCdf53IncompleteTile> = self
            .inner
            .borrow()
            .cdf53_incomplete_tiles(limit)
            .into_iter()
            .map(Into::into)
            .collect();
        to_js(&v)
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
