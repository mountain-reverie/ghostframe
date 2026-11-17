// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser client (port of `main.ts`'s `main()`).

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::{Rc, Weak};

use ghostframe_client_core::decode_error_batcher::DecodeErrorBatcher;
use ghostframe_client_core::{ClientCore, PollOutput};
use ghostframe_web_ui::CoreBackend;
use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{console, Document, HtmlCanvasElement, HtmlElement, WritableStreamDefaultWriter};

use crate::app::batch::{flush_decision, DatagramBatch, FlushDecision, FLUSH_FALLBACK_MS};
use crate::app::bootstrap::bootstrap;
use crate::app::close::close_overlay_message;
use crate::app::debug_hooks::{self, DebugHandles};
use crate::app::decoder::{not_output_warning, FrameDecoder};
use crate::app::dom::{page_flags, pin_canvas, show_disconnected_overlay, PageLog};
use crate::app::hooks::{HookCall, HookSink};
use crate::app::input::attach_input;
use crate::app::js::{StreamPair, WebTransport};
use crate::app::page::PageFlags;
use crate::app::recorder::{LogEntry, Recorder};
use crate::app::session::{nacks_in, Action, Drained, Session};
use crate::app::stats_line::{StatsInputs, StatsLine};
use crate::app::streams::{pump_stream, LoggedWriter, RepairStatusStream};
use crate::app::webcodecs::{frame_seq_of, OnError, OnOutput, WebCodec};
use crate::core::{client_config, datagram_counts, feed_datagrams, new_client_core};
use crate::web::raw::{close_frame, js_message};
use crate::web::renderer::WebRenderer;
use crate::web::route::{route, Routed};

/// `None` for an unrecognised discriminant: the caller drops the report
/// rather than panicking, since a panic aborts the whole wasm module.
///
/// `Codec::from_u8` returns `Result<Codec, ProtocolError>`; the error carries
/// nothing the caller acts on, so it is discarded.
fn codec_from_u8(v: u8) -> Option<ghostframe_protocol::protocol::Codec> {
    ghostframe_protocol::protocol::Codec::from_u8(v).ok()
}

fn decode_error_code_from_u8(v: u8) -> Option<ghostframe_client_core::DecodeErrorCode> {
    use ghostframe_client_core::DecodeErrorCode::*;
    Some(match v {
        1 => PayloadTooShort,
        2 => CountOutOfRange,
        3 => ThinUncachedPalette,
        4 => BundledTruncated,
        5 => IndexOob,
        6 => RleOvershoot,
        7 => RleUndershoot,
        8 => Cdf53BadPass,
        9 => Cdf53Truncated,
        10 => Cdf53RleLength,
        _ => return None,
    })
}

thread_local! {
    static PERFORMANCE: Option<web_sys::Performance> =
        web_sys::window().and_then(|w| w.performance());
    /// What consumes the hook calls. The one global: a decoder callback fires
    /// from the browser's task queue with nothing but a `Weak<App>` in hand,
    /// and must reach the sink after it has released its borrow. Set once by
    /// `start_client`, before any callback can exist.
    static SINK: RefCell<Option<Rc<dyn HookSink>>> = const { RefCell::new(None) };
}

/// `performance.now()`, in milliseconds.
fn now_ms() -> f64 {
    PERFORMANCE.with(|p| p.as_ref().map_or(0.0, |p| p.now()))
}

/// The microsecond clock every `now_us` parameter of the core takes.
fn now_us() -> u64 {
    (now_ms() * 1000.0).round() as u64
}

/// Run queued hook calls. Call with no `App` borrow held.
fn dispatch_calls(calls: Vec<HookCall>) {
    if calls.is_empty() {
        return;
    }
    let sink = SINK.with(|s| s.borrow().clone());
    if let Some(sink) = sink {
        sink.dispatch(calls);
    }
}

fn document() -> Option<Document> {
    web_sys::window().and_then(|w| w.document())
}

/// Everything one session's steps need. Borrowed by exactly one browser task
/// at a time (the rAF tick, the datagram reader, the fallback timer, a decoder
/// callback); hooks are dispatched after the borrow is released.
pub struct App {
    pub(crate) flags: PageFlags,
    pub(crate) renderer: Rc<RefCell<WebRenderer>>,
    pub(crate) core: Rc<RefCell<ClientCore>>,
    pub(crate) log: Rc<PageLog>,
    session: Session,
    /// ACK and NACK datagrams.
    datagrams: LoggedWriter,
    /// The feedback stream: HELLO, receiver feedback, decode errors, resets.
    /// `None` when the bidi stream could not be opened.
    feedback: Option<LoggedWriter>,
    decode_error_feedback: Option<LoggedWriter>,
    /// `PollOutput::RepairStatus`: its own stream, tagged, reopened after a
    /// failure. `None` only where a test has no transport.
    repair_status: Option<RepairStatusStream>,
    batch: DatagramBatch,
    /// The armed fallback timer's handle. Shared with `timer_cb`, which clears
    /// it before borrowing the app, so a fired timer never stays recorded as
    /// armed even when that borrow fails.
    flush_timer: Rc<Cell<Option<i32>>>,
    /// The fallback timer's one callback (a persistent closure: a fresh one
    /// per arming would be freed while the browser still holds it).
    timer_cb: Closure<dyn FnMut()>,
    decoder: Option<FrameDecoder<WebCodec>>,
    decode_errors: DecodeErrorBatcher,
    nack_sent: u32,
    raf_ticks: u32,
    in_tick: bool,
    out_of_tick_us: f64,
    tick_us: Rc<RefCell<Vec<u32>>>,
    calls: Vec<HookCall>,
    me: Weak<RefCell<App>>,
}

/// What `App::new` takes (the browser test builds one with fake streams).
pub struct AppParts {
    pub flags: PageFlags,
    pub renderer: Rc<RefCell<WebRenderer>>,
    pub core: Rc<RefCell<ClientCore>>,
    pub log: Rc<PageLog>,
    pub datagram_writer: WritableStreamDefaultWriter,
    pub feedback_writer: Option<WritableStreamDefaultWriter>,
    pub repair_status: Option<RepairStatusStream>,
    pub tick_us: Rc<RefCell<Vec<u32>>>,
}

impl Drop for App {
    fn drop(&mut self) {
        // A timer that outlives the closure it calls would throw in the page.
        self.clear_flush_timer();
    }
}

impl App {
    /// `Rc::new_cyclic`, so decoder callbacks can find the app.
    pub fn new(parts: AppParts) -> Rc<RefCell<App>> {
        Rc::new_cyclic(|me: &Weak<RefCell<App>>| {
            let timer_me = me.clone();
            let flush_timer: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
            let timer_handle = flush_timer.clone();
            let timer_cb = Closure::<dyn FnMut()>::new(move || {
                // It has fired: forget its handle first, so the next datagram
                // arms a fresh fallback even if the borrow below fails.
                timer_handle.set(None);
                let Some(app) = timer_me.upgrade() else {
                    return;
                };
                let calls = match app.try_borrow_mut() {
                    Ok(mut a) => {
                        a.flush_datagrams();
                        a.take_calls()
                    }
                    // Unreachable today (every borrow is synchronous); the
                    // batch waits for the next tick or the next arming.
                    Err(_) => return,
                };
                dispatch_calls(calls);
            });
            let feedback = parts
                .feedback_writer
                .map(|w| LoggedWriter::new(w, "feedback stream write failed"));
            let decode_error_feedback = feedback
                .as_ref()
                .map(|f| f.labelled("decode-error feedback write failed"));
            RefCell::new(App {
                flags: parts.flags,
                renderer: parts.renderer,
                core: parts.core,
                log: parts.log,
                session: Session::default(),
                datagrams: LoggedWriter::new(
                    parts.datagram_writer,
                    "ACK/NACK datagram write failed",
                ),
                feedback,
                decode_error_feedback,
                repair_status: parts.repair_status,
                batch: DatagramBatch::default(),
                flush_timer,
                timer_cb,
                decoder: None,
                decode_errors: DecodeErrorBatcher::new(),
                nack_sent: 0,
                raf_ticks: 0,
                in_tick: false,
                out_of_tick_us: 0.0,
                tick_us: parts.tick_us,
                calls: Vec::new(),
                me: me.clone(),
            })
        })
    }

    fn push(&mut self, call: HookCall) {
        self.calls.push(call);
    }

    /// Hook calls queued since the last take; dispatch them after the borrow ends.
    pub fn take_calls(&mut self) -> Vec<HookCall> {
        std::mem::take(&mut self.calls)
    }

    /// Drain every pending outbound buffer to its wire (the HELLO first, at
    /// startup, before any input listener exists).
    pub fn drain_transmit(&mut self) {
        loop {
            // A statement of its own, so the core's borrow ends before the write.
            let out = self.core.borrow_mut().poll_transmit(now_us());
            let Some(out) = out else { break };
            match out {
                PollOutput::Datagram(b) => {
                    // NACK envelopes arrive here too (same writer as ACK
                    // batches): the fec-coverage `nack_sent` counter.
                    self.nack_sent = self.nack_sent.saturating_add(nacks_in(&b));
                    self.datagrams.write(&b);
                }
                PollOutput::Stream(b) => {
                    if let Some(f) = &self.feedback {
                        f.write(&b);
                    }
                }
                PollOutput::RepairStatus(b) => {
                    if let Some(r) = &self.repair_status {
                        r.write(&b);
                    }
                }
            }
        }
    }

    /// One datagram off the WebTransport reader. `arrival_us` is
    /// `performance.now() * 1000` at the read.
    pub fn on_datagram(&mut self, value: &Uint8Array, arrival_us: f64, hidden: bool) {
        let len = value.length() as usize;
        if len == 0 {
            return;
        }
        // Every datagram is a rev 6 FEC frame (a pong is one too, and the
        // core ignores its record): nothing is handled before the core.
        // Queue, don't process: the batch crosses into the core once per rAF
        // tick (or sooner under backlog) instead of once per datagram.
        self.batch
            .push_with(len, arrival_us, |dst| value.copy_to(dst));
        // Hidden tab: rAF is paused and timers are clamped to >= 1 s, so
        // process at once to keep ACKs prompt.
        match flush_decision(self.batch.len(), hidden, self.flush_timer.get().is_some()) {
            FlushDecision::Now => self.flush_datagrams(),
            FlushDecision::ArmTimer => {
                if let Some(w) = web_sys::window() {
                    if let Ok(h) = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                        self.timer_cb.as_ref().unchecked_ref(),
                        FLUSH_FALLBACK_MS,
                    ) {
                        self.flush_timer.set(Some(h));
                    }
                }
            }
            FlushDecision::Wait => {}
        }
    }

    fn clear_flush_timer(&mut self) {
        if let Some(h) = self.flush_timer.take() {
            if let Some(w) = web_sys::window() {
                w.clear_timeout_with_handle(h);
            }
        }
    }

    /// Hand the batch to the core (the tick, the backlog, the timer, a hidden tab).
    pub fn flush_datagrams(&mut self) {
        let t0 = now_ms();
        self.clear_flush_timer();
        if self.batch.is_empty() {
            return;
        }
        let events = {
            let (c, l, a) = self.batch.parts();
            feed_datagrams(&mut self.core.borrow_mut(), c, l, a)
        };
        self.batch.clear();
        // The renderer's borrow ends with this statement.
        let routed = route(events, &mut *self.renderer.borrow_mut());
        self.handle_routed(routed);
        // Re-published from the core, which classifies tile vs frame itself.
        let (tile, frame) = datagram_counts(&self.core.borrow());
        self.push(HookCall::DatagramCounts { tile, frame });
        self.drain_transmit();
        if !self.in_tick {
            self.out_of_tick_us += (now_ms() - t0) * 1000.0;
        }
    }

    fn handle_routed(&mut self, routed: Vec<Routed>) {
        let mut records = Vec::new();
        for r in routed {
            let mut actions = Vec::new();
            let back = self.session.on_routed(r, &mut actions);
            records.extend(back);
            self.run_actions(actions);
        }
        if !records.is_empty() {
            self.push(HookCall::Events {
                at_ms: now_ms(),
                records,
            });
        }
    }

    fn run_actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Status(s) => self.log.status(s),
                Action::Log(l) => self.log.line(&l),
                Action::ConsoleError(m) => console::error_1(&m.into()),
                Action::Overlay(m) => {
                    if let Some(d) = document() {
                        show_disconnected_overlay(&d, m);
                    }
                }
                Action::DecodeH264 {
                    frame_seq,
                    is_keyframe,
                    payload,
                } => self.decode_h264(frame_seq, is_keyframe, &payload),
            }
        }
    }

    fn swallowed(&mut self, message: String) {
        let site = "handleEvent:NeedsH264";
        // Without `?e2e` there is no recorder: a production error must not be
        // silent. Under `?e2e` the health record keeps it.
        if !self.flags.e2e {
            console::error_3(
                &"handleEvent failed; continuing".into(),
                &site.into(),
                &message.as_str().into(),
            );
        }
        self.push(HookCall::Swallowed {
            site: site.into(),
            message,
        });
    }

    /// No `drain_transmit` here: the frame ack must leave behind the ACK batch,
    /// which the tick's flush carries.
    fn decode_h264(&mut self, frame_seq: u32, key: bool, payload: &[u8]) {
        if self.decoder.is_none() {
            let (on_output, on_error) = self.decoder_callbacks();
            match FrameDecoder::new(WebCodec::new(on_output, on_error)) {
                Ok(d) => self.decoder = Some(d),
                Err(e) => {
                    self.swallowed(e);
                    return;
                }
            }
        }
        if let Some(d) = self.decoder.as_mut() {
            if let Err(e) = d.decode(payload, key, frame_seq) {
                self.swallowed(e);
            }
        }
        self.apply_decoder_notes();
    }

    /// The decoder's callbacks run from the browser's task queue, never inside
    /// a step, so the app is free; on a failure to borrow the frame is closed
    /// rather than leaked, and nothing panics.
    fn decoder_callbacks(&self) -> (OnOutput, OnError) {
        let me = self.me.clone();
        let on_output: OnOutput = Rc::new(move |generation, frame| {
            let Some(app) = me.upgrade() else {
                close_frame(&frame);
                return;
            };
            let calls = match app.try_borrow_mut() {
                Ok(mut a) => {
                    a.on_decoder_output(generation, frame);
                    a.take_calls()
                }
                Err(_) => {
                    close_frame(&frame);
                    console::error_1(
                        &"decoder output while the client was busy; frame closed".into(),
                    );
                    return;
                }
            };
            dispatch_calls(calls);
        });
        let me = self.me.clone();
        let on_error: OnError = Rc::new(move |generation, message| {
            let Some(app) = me.upgrade() else { return };
            let calls = match app.try_borrow_mut() {
                Ok(mut a) => {
                    a.on_decoder_error(generation, message);
                    a.take_calls()
                }
                Err(_) => {
                    console::error_1(&"decoder error while the client was busy".into());
                    return;
                }
            };
            dispatch_calls(calls);
        });
        (on_output, on_error)
    }

    /// Tell the core what the decoder did with each access unit: the reference
    /// gate, the frame ack and, after a failure, the paced IDR request hang off it.
    fn apply_decoder_notes(&mut self) {
        let Some(d) = self.decoder.as_mut() else {
            return;
        };
        let notes = d.take_notes();
        let warnings = d.take_warnings();
        for (seq, outcome) in notes {
            let mut core = self.core.borrow_mut();
            core.h264_result(seq, outcome, now_us());
            // A decoder that delays its output answers every frame not-output
            // while the stream plays; say so once when the run gets long.
            let (total, streak) = core.h264_not_output_stats();
            if let Some(w) = not_output_warning(total, streak) {
                console::warn_1(&w.into());
            }
        }
        for w in warnings {
            console::warn_1(&w.into());
        }
    }

    /// A decoder output (see `WebCodec`).
    pub fn on_decoder_output(&mut self, generation: u32, frame: JsValue) {
        let seq = frame_seq_of(&frame);
        let wanted = self
            .decoder
            .as_mut()
            .is_some_and(|d| d.on_output(generation, seq));
        if wanted {
            self.renderer.borrow_mut().push_h264(frame, seq);
        } else {
            close_frame(&frame);
        }
        self.apply_decoder_notes();
    }

    pub fn on_decoder_error(&mut self, generation: u32, message: String) {
        console::error_2(&"Full-frame H264 decode error:".into(), &message.into());
        if let Some(d) = self.decoder.as_mut() {
            d.on_error(generation);
        }
        self.apply_decoder_notes();
    }

    /// One animation frame.
    pub fn tick(&mut self) {
        self.raf_ticks += 1;
        let t0 = now_ms();
        self.in_tick = true;
        self.push(HookCall::RafTick(self.raf_ticks));

        // Assembly-timeout NACKs, the tail sweep and the periodic feedback/ACK
        // flush deadlines are internal to `ClientCore::on_timeout`.
        self.flush_datagrams();
        let events = self.core.borrow_mut().on_timeout(now_us());
        let routed = route(events, &mut *self.renderer.borrow_mut());
        self.handle_routed(routed);
        self.drain_transmit();

        self.push(HookCall::BeforePaint { at_ms: now_ms() });
        // Flush the renderer's tiles, draw pending H.264 pictures, present; its
        // errors stay for `take_error_strings`.
        let painted = self.renderer.borrow_mut().paint_tick_js(now_us());
        let (paint_ticks, errors, decode_errors) = {
            let mut r = self.renderer.borrow_mut();
            (
                r.paint_tick_count(),
                r.take_error_strings(),
                r.take_decode_errors(),
            )
        };
        for message in errors {
            self.push(HookCall::RendererError {
                tick: paint_ticks,
                message,
            });
        }
        // Tiles the GPU decode rejected: `[codec, tile_x, tile_y, code]` quads.
        for q in decode_errors.chunks_exact(4) {
            let (Ok(codec), Ok(x), Ok(y), Ok(code)) = (
                u8::try_from(q[0]),
                u8::try_from(q[1]),
                u8::try_from(q[2]),
                u8::try_from(q[3]),
            ) else {
                continue;
            };
            let (Some(codec), Some(code)) = (codec_from_u8(codec), decode_error_code_from_u8(code))
            else {
                continue;
            };
            if let Some(bytes) = self.decode_errors.report(codec, x, y, code, now_us()) {
                if let Some(f) = &self.decode_error_feedback {
                    f.write(&bytes);
                }
            }
        }
        // Frames actually presented, for the stats panel's painted-fps row.
        if painted > 0 {
            self.core.borrow_mut().record_paints(painted);
        }
        let drained = self.session.take_drained();
        self.push(HookCall::AfterPaint {
            drained,
            raf_ticks: self.raf_ticks,
            nack_sent: self.nack_sent,
        });
        self.in_tick = false;
        // Per presenting tick: flushes made before a tick that presents
        // nothing are dropped with it.
        if self.flags.e2e && painted > 0 {
            let us = (now_ms() - t0) * 1000.0 + self.out_of_tick_us;
            if let Ok(mut v) = self.tick_us.try_borrow_mut() {
                v.push(us.round() as u32);
            }
        }
        self.out_of_tick_us = 0.0;
    }

    /// The transport closed: renderer first (it closes the VideoFrames it owns),
    /// then the decoder.
    pub fn on_session_reset(&mut self) {
        self.renderer.borrow_mut().on_session_reset();
        if let Some(mut d) = self.decoder.take() {
            d.close();
        }
    }
}

/// What `start_client` hands the hook calls to: the e2e recorder (`Some`
/// under `?e2e`) and the page's stats line. Each borrow is a
/// `try_borrow_mut`: on a conflict the call is dropped and logged once.
struct PageSink {
    recorder: Option<Rc<RefCell<Recorder>>>,
    /// The `window` logs `recorder`'s entries go to (`?e2e` only).
    logs: Option<debug_hooks::Logs>,
    warned: Cell<bool>,
    stats: RefCell<StatsLine>,
    renderer: Rc<RefCell<WebRenderer>>,
    core: Rc<OnceCell<Rc<RefCell<ClientCore>>>>,
    log: Rc<PageLog>,
    e2e: bool,
}

impl PageSink {
    fn conflict(&self) {
        if !self.warned.replace(true) {
            console::error_1(&"hook sink busy; a hook call was dropped".into());
        }
    }

    /// One `AfterPaint`: the ~2 s stats line, and under `?e2e` the
    /// framebuffer probe when lines were logged.
    fn stats_tick(&self, drained: Drained, raf_ticks: u32, nack_sent: u32) {
        // The drain counts go in first: a tick that cannot build a line
        // (no core yet, a borrow conflict) must not lose them.
        match self.stats.try_borrow_mut() {
            Ok(mut stats) => stats.add_drained(drained),
            Err(_) => {
                self.conflict();
                return;
            }
        }
        let Some(core) = self.core.get() else { return };
        let (Ok(r), Ok(core)) = (self.renderer.try_borrow(), core.try_borrow()) else {
            self.conflict();
            return;
        };
        let fb = (r.fb_width(), r.fb_height());
        let summary = core.cdf53_coverage_summary();
        let coverage_line = summary.to_log_line();
        let incomplete: Vec<(u8, u8, u16, u32)> = if summary.partial > 0 || summary.gave_up > 0 {
            core.cdf53_incomplete_tiles(8)
                .into_iter()
                .map(|(x, y, mask, present, sweeps)| (x, y, present & !mask, u32::from(sweeps)))
                .collect()
        } else {
            Vec::new()
        };
        let inputs = StatsInputs {
            now_ms: now_ms(),
            raf_ticks,
            submit_count: r.submit_count(),
            fb,
            nack_sent,
            coverage_line: &coverage_line,
            partial: u64::from(summary.partial),
            gave_up: u64::from(summary.gave_up),
            incomplete: &incomplete,
        };
        let Ok(mut stats) = self.stats.try_borrow_mut() else {
            self.conflict();
            return;
        };
        let Some(lines) = stats.on_tick(&inputs) else {
            return;
        };
        drop(stats);
        for l in &lines {
            self.log.line(l);
        }
        if self.e2e && fb.0 > 0 && fb.1 > 0 {
            self.probe(&r, fb);
        }
    }

    /// Four known framebuffer points, to settle "is the texture written or is
    /// the blit broken". The copies are submitted now; the log waits for them.
    fn probe(&self, r: &WebRenderer, fb: (u32, u32)) {
        const XS: [u32; 4] = [16, 960, 768, 1900];
        const YS: [u32; 4] = [16, 540, 992, 1060];
        let mut promises = Vec::with_capacity(4);
        for (x, y) in XS.iter().zip(YS) {
            match r.read_pixel_rect(*x.min(&(fb.0 - 1)), y.min(fb.1 - 1), 1, 1) {
                Ok(p) => promises.push(p),
                Err(e) => {
                    self.log
                        .line(&format!("fb-pixels readback failed: {}", fmt_js(&e)));
                    return;
                }
            }
        }
        let log = self.log.clone();
        spawn_local(async move {
            let mut px: Vec<String> = Vec::with_capacity(4);
            for p in promises {
                match JsFuture::from(p).await {
                    Ok(v) => {
                        let b = Uint8Array::new(&v).to_vec();
                        if b.len() < 4 {
                            log.line(&format!("fb-pixels readback failed: {} bytes", b.len()));
                            return;
                        }
                        px.push(format!("[{},{},{},{}]", b[0], b[1], b[2], b[3]));
                    }
                    Err(e) => {
                        log.line(&format!("fb-pixels readback failed: {}", fmt_js(&e)));
                        return;
                    }
                }
            }
            log.line(&format!(
                "fb-pixels: tl(16,16)={} ctr(960,540)={} tile(768,992)={} br(1900,1060)={}",
                px[0], px[1], px[2], px[3]
            ));
        });
    }
}

impl HookSink for PageSink {
    fn dispatch(&self, calls: Vec<HookCall>) {
        let mut out: Vec<LogEntry> = Vec::new();
        for call in calls {
            if let HookCall::Events { records, .. } = &call {
                match self.stats.try_borrow_mut() {
                    Ok(mut stats) => stats.note_events(records),
                    Err(_) => self.conflict(),
                }
            }
            let after = match &call {
                HookCall::AfterPaint {
                    drained,
                    raf_ticks,
                    nack_sent,
                } => Some((*drained, *raf_ticks, *nack_sent)),
                HookCall::Events { .. }
                | HookCall::DatagramCounts { .. }
                | HookCall::RafTick(_)
                | HookCall::BeforePaint { .. }
                | HookCall::Swallowed { .. }
                | HookCall::RendererError { .. }
                | HookCall::ServerStream { .. } => None,
            };
            if let Some(rec) = &self.recorder {
                match rec.try_borrow_mut() {
                    Ok(mut r) => r.apply(call, &mut out),
                    Err(_) => self.conflict(),
                }
            }
            if let Some((d, raf, nack)) = after {
                self.stats_tick(d, raf, nack);
            }
        }
        if let Some(logs) = &self.logs {
            debug_hooks::append(logs, out);
        }
    }
}

/// Start the client on this page. Resolves when the datagram stream ends; an
/// error is shown on the page, not thrown.
#[wasm_bindgen(js_name = startClient)]
pub async fn start_client() -> Result<(), JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let log = Rc::new(PageLog::find(&document).map_err(|e| JsValue::from_str(&e))?);
    // Under `?e2e` the recorder exists from here, and the console ring is the
    // first thing installed, so even a start-up failure is in the dump.
    let recorder = page_flags(&window)
        .e2e
        .then(|| Rc::new(RefCell::new(Recorder::new(true))));
    if let Some(rec) = &recorder {
        debug_hooks::install_console_ring(rec);
    }
    if let Err(msg) = run(&window, &document, log.clone(), recorder).await {
        log.line(&format!("Error: {msg}"));
        log.status(&format!("Error: {msg}"));
    }
    Ok(())
}

fn fmt_js(v: &JsValue) -> String {
    if v.is_undefined() {
        "undefined".into()
    } else if let Some(s) = v.as_string() {
        s
    } else if let Some(n) = v.as_f64() {
        n.to_string()
    } else {
        js_message(v)
    }
}

async fn run(
    window: &web_sys::Window,
    document: &Document,
    log: Rc<PageLog>,
    recorder: Option<Rc<RefCell<Recorder>>>,
) -> Result<(), String> {
    let flags = page_flags(window);
    let canvas: HtmlCanvasElement = document
        .get_element_by_id("canvas")
        .and_then(|e| e.dyn_into().ok())
        .ok_or("#canvas is missing from index.html")?;

    // ?e2e=lossless: strip page chrome so a CDP screenshot captures only the
    // canvas, top-left aligned. Status and log are still wired up internally.
    if flags.lossless {
        log.hide();
        pin_canvas(&canvas);
    }

    let origin = window.location().origin().map_err(|e| js_message(&e))?;
    log.line(&format!("Connecting to {origin}..."));

    // WebGPU init: fatal if unavailable (design D2).
    let renderer = match WebRenderer::create_inner(canvas.clone(), flags.cdf53).await {
        Ok(r) => r,
        Err(e) if e.starts_with("WEBGPU_UNAVAILABLE") => {
            log.status("WebGPU not available in this browser.");
            log.line(&e);
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let h264_supported = renderer.h264_supported();
    let renderer = Rc::new(RefCell::new(renderer));

    let api_core: Rc<OnceCell<Rc<RefCell<ClientCore>>>> = Rc::new(OnceCell::new());
    let tick_us = Rc::new(RefCell::new(Vec::new()));
    // Before bootstrap, so a failure dump still finds `__ghostframeHealth`.
    let logs = match (&recorder, flags.e2e) {
        (Some(rec), true) => Some(debug_hooks::install(
            window,
            DebugHandles {
                renderer: renderer.clone(),
                core: api_core.clone(),
                recorder: rec.clone(),
                tick_us: tick_us.clone(),
            },
        )),
        _ => None,
    };
    let sink: Rc<dyn HookSink> = Rc::new(PageSink {
        recorder,
        logs,
        warned: Cell::new(false),
        stats: RefCell::new(StatsLine::new()),
        renderer: renderer.clone(),
        core: api_core.clone(),
        log: log.clone(),
        e2e: flags.e2e,
    });
    SINK.with(|s| *s.borrow_mut() = Some(sink));

    let transport: WebTransport = bootstrap().await?;

    // The closed handler waits on this: it never borrows an app that does not
    // exist yet.
    let app_cell: Rc<OnceCell<Rc<RefCell<App>>>> = Rc::new(OnceCell::new());
    {
        let closed = transport.closed();
        let (app_cell, renderer, log, document) = (
            app_cell.clone(),
            renderer.clone(),
            log.clone(),
            document.clone(),
        );
        spawn_local(async move {
            let result = JsFuture::from(closed).await;
            match &result {
                Ok(info) => {
                    let code =
                        Reflect::get(info, &"closeCode".into()).unwrap_or(JsValue::UNDEFINED);
                    let reason = Reflect::get(info, &"reason".into()).unwrap_or(JsValue::UNDEFINED);
                    log.line(&format!(
                        "Transport closed: code={} reason={}",
                        fmt_js(&code),
                        fmt_js(&reason)
                    ));
                }
                Err(e) => log.line(&format!("Transport closed with error: {}", js_message(e))),
            }
            // The periodic feedback writer is gone: the core emits
            // ReceiverFeedback from on_timeout, driven by the rAF loop, which
            // stops on its own when the session tears down.
            match app_cell.get() {
                Some(app) => app.borrow_mut().on_session_reset(),
                // The session ended before the app existed: the renderer is all
                // there is to reset.
                None => {
                    if let Ok(mut r) = renderer.try_borrow_mut() {
                        r.on_session_reset();
                    }
                }
            }
            if let Ok(info) = &result {
                let code = Reflect::get(info, &"closeCode".into())
                    .ok()
                    .and_then(|c| c.as_f64())
                    .map(|c| c as u32);
                if let Some(msg) = close_overlay_message(code) {
                    show_disconnected_overlay(&document, msg);
                }
            }
        });
    }

    JsFuture::from(transport.ready())
        .await
        .map_err(|e| js_message(&e))?;
    log.line("Connected!");
    log.status("Connected");

    // The feedback bidi stream: HELLO (one-shot), ReceiverFeedback (periodic),
    // DECODE_ERROR (rate-limited, on demand). Optional: every write site
    // treats its absence as "no feedback path".
    let bidi = match JsFuture::from(transport.create_bidirectional_stream()).await {
        Ok(b) => Some(b.unchecked_into::<StreamPair>()),
        Err(_) => {
            console::warn_1(&"Could not open feedback stream".into());
            None
        }
    };
    let feedback_writer = match &bidi {
        Some(b) => Some(b.writable().get_writer().map_err(|e| js_message(&e))?),
        None => None,
    };
    let datagram_writer = transport
        .datagrams()
        .writable()
        .get_writer()
        .map_err(|e| js_message(&e))?;

    // `indices_raw_enabled` is unconditionally true (WebGPU is required);
    // `supports_h264` is the renderer's startup probe; `tile_delivery_payload`
    // is unconditionally true: it hands the GPU validated-but-undecoded
    // payloads. Passing false would route tiles down the native Decoded path
    // and hand back RGBA instead, which looks like it works and is wrong.
    let core = Rc::new(RefCell::new(new_client_core(
        client_config(true, h264_supported, true),
        now_us(),
    )));
    let _ = api_core.set(core.clone());

    let input_writer = feedback_writer
        .clone()
        .map(|w| Rc::new(LoggedWriter::new(w, "input send failed")));
    let app = App::new(AppParts {
        flags,
        renderer: renderer.clone(),
        core: core.clone(),
        log: log.clone(),
        datagram_writer,
        feedback_writer,
        repair_status: Some(RepairStatusStream::new({
            let transport: WebTransport = JsValue::from(transport.clone()).unchecked_into();
            Rc::new(move || transport.create_bidirectional_stream())
        })),
        tick_us,
    });
    let _ = app_cell.set(app.clone());

    // Flush the HELLO the core queued at construction, before any input
    // listener exists, so no input event can beat it onto the feedback stream.
    app.borrow_mut().drain_transmit();

    // Stats panel. A broken panel must never break the stream. `?e2e=lossless`
    // strips the page chrome, so no panel there.
    if !flags.lossless {
        let mounted = document
            .get_element_by_id("stats-panel")
            .and_then(|e| e.dyn_into::<HtmlElement>().ok())
            .ok_or_else(|| JsValue::from_str("#stats-panel is missing from index.html"))
            .and_then(|host| {
                ghostframe_web_ui::mount_stats_panel(host, Rc::new(CoreBackend::new(core.clone())))
            });
        if let Err(e) = mounted {
            console::warn_2(
                &"stats panel failed to mount; continuing without it".into(),
                &e,
            );
        }
    }

    // The server writes stats on the feedback stream's return half. Nothing is
    // sent until the panel subscribes, so this is idle by default.
    if let Some(b) = &bidi {
        let (core, e2e) = (core.clone(), flags.e2e);
        spawn_local(pump_stream(
            b.readable(),
            move |bytes| {
                let (samples, frames) = {
                    let mut c = core.try_borrow_mut().map_err(|_| {
                        // Every core borrow is synchronous, so this cannot
                        // happen; if it does, the stats stream's reassembly
                        // is broken from here on, not one chunk skipped.
                        "invariant violated: the core was borrowed while a server-stream \
                         chunk arrived; chunk dropped, stats stream may be corrupt"
                            .to_string()
                    })?;
                    c.handle_server_stream(bytes, now_us());
                    let s = c.stats();
                    (
                        s.samples_accepted().min(u64::from(u32::MAX)) as u32,
                        s.frames_received().min(u64::from(u32::MAX)) as u32,
                    )
                };
                if e2e {
                    dispatch_calls(vec![HookCall::ServerStream { samples, frames }]);
                }
                Ok(())
            },
            |what, e| console::warn_2(&what.into(), &e.into()),
        ));
    }

    // Browser to server input, through the same feedback writer the HELLO used.
    if let Some(w) = input_writer {
        attach_input(&canvas, w, renderer.clone());
    }

    // The rAF loop: the standard self-rescheduling closure. It lives for the page.
    {
        let app = app.clone();
        let f: FrameLoop = Rc::new(RefCell::new(None));
        let g = f.clone();
        *g.borrow_mut() = Some(Closure::new(move |_t: f64| {
            let calls = {
                let mut a = app.borrow_mut();
                a.tick();
                a.take_calls()
            };
            dispatch_calls(calls);
            request_frame(&f);
        }));
        request_frame(&g);
    }

    // Receive datagrams: the read loop only queues; the tick (or the backlog or
    // timer fallback) hands the batch to the core.
    let reader: web_sys::ReadableStreamDefaultReader = transport
        .datagrams()
        .readable()
        .get_reader()
        .unchecked_into();
    loop {
        let result = JsFuture::from(reader.read())
            .await
            .map_err(|e| js_message(&e))?;
        if Reflect::get(&result, &"done".into())
            .ok()
            .and_then(|d| d.as_bool())
            == Some(true)
        {
            break;
        }
        let Some(value) = Reflect::get(&result, &"value".into())
            .ok()
            .and_then(|v| v.dyn_into::<Uint8Array>().ok())
        else {
            continue;
        };
        let calls = {
            let mut a = app.borrow_mut();
            a.on_datagram(&value, now_ms() * 1000.0, document.hidden());
            a.take_calls()
        };
        dispatch_calls(calls);
    }
    Ok(())
}

/// The self-rescheduling rAF closure's cell.
type FrameLoop = Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>;

fn request_frame(f: &FrameLoop) {
    if let (Some(w), Some(c)) = (web_sys::window(), f.borrow().as_ref()) {
        let _ = w.request_animation_frame(c.as_ref().unchecked_ref());
    }
}
