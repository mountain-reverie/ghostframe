// SPDX-License-Identifier: MIT OR Apache-2.0
//! The decode thread: turns the net thread's tile payloads into pixels.
//!
//! The net thread runs `ClientNet` in `TileDelivery::Payload` mode, so it
//! does only transport and protocol work -- QUIC, reassembly, validation,
//! coverage, ACKs and NACKs. Decoding a CDF 5/3 tile (bit-plane decode plus
//! an inverse wavelet transform) is the heaviest CPU work in the client, and
//! on the net thread it delayed every timer and acknowledgement behind it.
//! Here it runs beside the transport instead of in its way, on another core.
//!
//! Every core event goes through this thread, not just tiles, so the render
//! thread still sees them in the order the core produced them (paint order
//! between tiles and H.264 frames depends on it). `TileDecoder` is stateful
//! and must see all of them in order anyway.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use ghostframe_client_core::{
    BatchStep, Decoded, Event as CoreEvent, NoSlots, TileData, TileDecoder,
};
use ghostframe_client_gpu::upload_pool::{SlotAllocator, SlotRef};

use crate::render_thread::RenderMsg;

/// Most messages held back before they are sent regardless. See [`Outbox`].
const OUTBOX_MAX: usize = 32;

/// Longest the oldest held message waits while input keeps arriving. A CDF 5/3
/// tile can take several milliseconds to decode on this hardware, so the count
/// alone does not bound the delay behind the first slab tile.
const OUTBOX_MAX_AGE: Duration = Duration::from_millis(1);

/// Most events decoded as one batch when the input is backed up. Within a
/// batch each CDF 5/3 tile is reconstructed once, for its newest pass.
///
/// The server's startup burst goes layer by layer: pass 0 of every tile, then
/// pass 1 of every tile, and so on, so the passes of one tile are as far apart
/// as there are tiles on the screen (about 2000 at 1080p). A window shorter
/// than that coalesces nothing (measured: 64 saved 0.3% of the
/// reconstructions), and the bound must span several layers. 4096 does, and
/// costs at most a few megabytes of queued events.
///
/// The price is delay, bounded as follows. A CDF 5/3 tile is drawn only at
/// the end of the batch (or when something must follow it), so its first
/// picture waits for the batch's accumulation, a 384-byte copy per pass, a few
/// milliseconds at most. A Solid or PalRle tile is decoded in place but may sit
/// in the [`Outbox`] behind that accumulation; [`BatchStep::Tick`] every 256
/// passes lets the outbox's age limit act, so it waits for about 256 copies
/// (well under a millisecond) beyond [`OUTBOX_MAX_AGE`], and a held tile never
/// waits through a reconstruction ([`BatchStep::BeforeReconstruct`]).
const BATCH_MAX: usize = 4096;

/// Messages produced since the allocator's CPU write window was last closed.
///
/// A message naming an upload slot tells the render thread it may hand that
/// slot to the GPU, so the slot's bytes must be complete by then: the write
/// window (`DMA_BUF_IOCTL_SYNC`, one ioctl per slab) is closed first, and that
/// is too costly to do per tile. So once a slab tile is pending, everything
/// behind it queues here too -- order is the whole contract of this thread --
/// and the lot is released together when the input runs dry, when
/// [`OUTBOX_MAX`] messages are held, or when the oldest has waited
/// [`OUTBOX_MAX_AGE`].
#[derive(Default)]
struct Outbox {
    held: Vec<RenderMsg>,
    /// When the first held message was held.
    since: Option<Instant>,
}

impl Outbox {
    /// `now` stamps the first held message. It must come from the same clock
    /// [`Outbox::due_at`] is later judged against: stamping with the wall
    /// clock here made an injected clock that lags real time (a test's, or a
    /// slow decode under load) see the age saturate to zero and never expire.
    fn push(&mut self, msg: RenderMsg, now: Instant) {
        self.since.get_or_insert(now);
        self.held.push(msg);
    }

    /// Whether the held messages have waited long enough (judged at `now`, so
    /// a test need not race the clock), or are enough.
    fn due_at(&self, now: Instant) -> bool {
        self.held.len() >= OUTBOX_MAX
            || self
                .since
                .is_some_and(|t| now.saturating_duration_since(t) >= OUTBOX_MAX_AGE)
    }

    /// Close the write window, then send everything held, in order.
    /// `false` once the render thread is gone.
    fn release(&mut self, slots: Option<&mut SlotAllocator>, tx: &Sender<RenderMsg>) -> bool {
        if let Some(slots) = slots {
            slots.end_cpu_writes();
        }
        self.since = None;
        self.held.drain(..).all(|m| tx.send(m).is_ok())
    }
}

/// The decode loop's state; [`run`] feeds it events. Split out so a test can
/// drive it with a clock of its own.
struct Pump {
    decoder: TileDecoder,
    outbox: Outbox,
    slots: Option<SlotAllocator>,
    render_tx: Sender<RenderMsg>,
    render_gone: bool,
    /// CDF 5/3 passes travel to the render thread as expanded planes (its GPU
    /// grid reconstructs them from those), so none is "heavy" here -- the
    /// RLE expansion is cheap -- and none is coalesced.
    cdf53_on_gpu: bool,
}

/// What the render thread is sent for a decoded result.
fn render_msg(decoded: Decoded<SlotRef>) -> RenderMsg {
    match decoded {
        Decoded::Event(ev) => RenderMsg::Core(ev),
        Decoded::Placed {
            frame_seq,
            tile_x,
            tile_y,
            placed,
        } => RenderMsg::SlabTile {
            frame_seq,
            tile_x,
            tile_y,
            slot: placed,
        },
    }
}

/// [`Outbox::release`], remembering in `render_gone` if the render thread is.
fn release_outbox(
    outbox: &mut Outbox,
    slots: Option<&mut SlotAllocator>,
    render_tx: &Sender<RenderMsg>,
    render_gone: &mut bool,
) {
    if !outbox.release(slots, render_tx) {
        // Keep draining so the net thread never blocks or errors on us;
        // its ACK/NACK timing must keep running until it is told to stop.
        tracing::warn!("render thread channel closed; dropping decoded events");
        *render_gone = true;
    }
}

/// Hold or send what one event decoded to, judging the age limit at `now`.
fn deliver(
    outbox: &mut Outbox,
    slots: Option<&mut SlotAllocator>,
    render_tx: &Sender<RenderMsg>,
    render_gone: &mut bool,
    decoded: Decoded<SlotRef>,
    now: Instant,
) {
    if *render_gone {
        return;
    }
    let msg = render_msg(decoded);
    let waits = !outbox.held.is_empty() || matches!(msg, RenderMsg::SlabTile { .. });
    outbox.push(msg, now);
    if !waits || outbox.due_at(now) {
        release_outbox(outbox, slots, render_tx, render_gone);
    }
}

impl Pump {
    /// Release what is held; remember if the render thread is gone.
    fn release(&mut self) {
        release_outbox(
            &mut self.outbox,
            self.slots.as_mut(),
            &self.render_tx,
            &mut self.render_gone,
        );
    }

    fn release_if_held(&mut self) {
        if !self.outbox.held.is_empty() {
            self.release();
        }
    }

    /// Decode one event and send, or hold, what it produces. `now` is the time
    /// the age limit is judged against.
    fn handle(&mut self, ev: CoreEvent, now: Instant) {
        if self.render_gone {
            // Decoding for nobody would only fill slabs and open write windows
            // that nothing closes.
            return;
        }
        // A CDF 5/3 pass takes milliseconds to decode; don't make what is
        // already finished wait for it.
        let heavy = !self.cdf53_on_gpu
            && matches!(
                &ev,
                CoreEvent::TilePayload {
                    data: TileData::Cdf53 { .. } | TileData::Cdf53Wire { .. },
                    ..
                }
            );
        if heavy {
            self.release_if_held();
            if self.render_gone {
                return;
            }
        }
        let decoded: Option<Decoded<SlotRef>> = match self.slots.as_mut() {
            Some(slots) => self.decoder.decode_into(ev, slots),
            None => self.decoder.decode(ev).map(Decoded::Event),
        };
        let Some(decoded) = decoded else {
            // Nothing to send, but time passed: a held batch must not wait for
            // the next message that does decode.
            if self.outbox.due_at(now) {
                self.release();
            }
            return;
        };
        deliver(
            &mut self.outbox,
            self.slots.as_mut(),
            &self.render_tx,
            &mut self.render_gone,
            decoded,
            now,
        );
    }

    /// [`Pump::handle`] for several events that were already queued, decoded
    /// as a batch: a CDF 5/3 tile with more than one pass in it is
    /// reconstructed once. Results are delivered as the decoder makes them, so
    /// the outbox's count and age limits still apply between them, and what
    /// is held is sent before each reconstruction, as `handle` does before a
    /// heavy pass. The age is judged by `clock` at each step, since a batch
    /// can outlast any one reading. Once the render thread is gone the rest
    /// of the batch is not decoded. Returns the time from the start of the
    /// batch to its first result, if it had one.
    fn handle_batch(
        &mut self,
        events: &mut Vec<CoreEvent>,
        clock: &mut impl FnMut() -> Instant,
    ) -> Option<Duration> {
        if self.render_gone {
            events.clear();
            return None;
        }
        let started = clock();
        let heavy = !self.cdf53_on_gpu
            && events.iter().any(|ev| {
                matches!(
                    ev,
                    CoreEvent::TilePayload {
                        data: TileData::Cdf53 { .. } | TileData::Cdf53Wire { .. },
                        ..
                    }
                )
            });
        if heavy {
            self.release_if_held();
            if self.render_gone {
                events.clear();
                return None;
            }
        }
        let mut first = None;
        let Pump {
            decoder,
            outbox,
            slots,
            render_tx,
            render_gone,
            cdf53_on_gpu: _,
        } = self;
        match slots.as_mut() {
            Some(slots) => decoder.decode_batch_into(events.drain(..), slots, |slots, step| {
                match step {
                    BatchStep::Out(d) => {
                        let now = clock();
                        first.get_or_insert_with(|| now.saturating_duration_since(started));
                        deliver(outbox, Some(&mut *slots), render_tx, render_gone, d, now);
                    }
                    BatchStep::BeforeReconstruct => {
                        if !outbox.held.is_empty() {
                            release_outbox(outbox, Some(slots), render_tx, render_gone);
                        }
                    }
                    BatchStep::Tick => {
                        if outbox.due_at(clock()) {
                            release_outbox(outbox, Some(slots), render_tx, render_gone);
                        }
                    }
                }
                !*render_gone
            }),
            None => decoder.decode_batch_into(events.drain(..), &mut NoSlots, |_, step| {
                match step {
                    BatchStep::Out(d) => {
                        let d = match d {
                            Decoded::Event(ev) => Decoded::Event(ev),
                            Decoded::Placed { placed, .. } => match placed {},
                        };
                        let now = clock();
                        first.get_or_insert_with(|| now.saturating_duration_since(started));
                        deliver(outbox, None, render_tx, render_gone, d, now);
                    }
                    BatchStep::BeforeReconstruct => {
                        if !outbox.held.is_empty() {
                            release_outbox(outbox, None, render_tx, render_gone);
                        }
                    }
                    BatchStep::Tick => {
                        if outbox.due_at(clock()) {
                            release_outbox(outbox, None, render_tx, render_gone);
                        }
                    }
                }
                !*render_gone
            }),
        }
        // `drain` leaves nothing behind even when the decoder stopped early.
        // Results that decoded to nothing still let time pass.
        if !*render_gone && outbox.due_at(clock()) {
            self.release();
        }
        first
    }
}

/// Decode until the net thread drops its end of `rx`.
///
/// `slots` is the write side of the render thread's upload slabs, if it made
/// any. With it, tiles are decoded straight into slab memory and travel as
/// [`RenderMsg::SlabTile`]; without it, or whenever every slab is busy, they
/// travel as `Vec`s in [`RenderMsg::Core`] exactly as before. Never blocks
/// waiting for a slot.
///
/// `cdf53_on_gpu`: CDF 5/3 passes are RLE-expanded here (a `Cdf53Wire` pass
/// from the net thread) and forwarded as planes, not reconstructed, for the
/// render thread's GPU grid (`GHOSTFRAME_CDF53_DECODE`, decided once at
/// start-up by the render thread's self-test). Everything else decodes here
/// as before.
pub(crate) fn run(
    rx: Receiver<CoreEvent>,
    render_tx: Sender<RenderMsg>,
    slots: Option<SlotAllocator>,
    cdf53_on_gpu: bool,
) {
    run_batched(rx, render_tx, slots, BATCH_MAX, cdf53_on_gpu)
}

/// [`run`] with the batch bound chosen, so that a test can cross it cheaply.
fn run_batched(
    rx: Receiver<CoreEvent>,
    render_tx: Sender<RenderMsg>,
    slots: Option<SlotAllocator>,
    batch_max: usize,
    cdf53_on_gpu: bool,
) {
    let mut pump = Pump {
        decoder: TileDecoder::new().with_cdf53_passthrough(cdf53_on_gpu),
        outbox: Outbox::default(),
        slots,
        render_tx,
        render_gone: false,
        cdf53_on_gpu,
    };
    let mut stats = Stats::new();
    let mut batch = Vec::with_capacity(batch_max.min(256));
    loop {
        let first = match rx.try_recv() {
            Ok(ev) => ev,
            Err(TryRecvError::Empty) => {
                // About to wait: nothing more is coming to batch with.
                pump.release_if_held();
                match rx.recv() {
                    Ok(ev) => ev,
                    Err(_) => break,
                }
            }
            Err(TryRecvError::Disconnected) => break,
        };
        match rx.try_recv() {
            // The usual case in steady state: exactly the one event, handled
            // as it always was. (The next turn asks again before waiting, so
            // an event that arrives during a slow decode joins a held outbox.)
            Err(TryRecvError::Empty) => {
                stats.count(&first);
                pump.handle(first, Instant::now());
                stats.tick(&pump.decoder);
            }
            Err(TryRecvError::Disconnected) => {
                pump.handle(first, Instant::now());
                break;
            }
            // Behind: take what is queued, up to the bound, and decode it
            // together.
            Ok(second) => {
                batch.push(first);
                batch.push(second);
                while batch.len() < batch_max {
                    match rx.try_recv() {
                        Ok(ev) => batch.push(ev),
                        Err(_) => break,
                    }
                }
                batch.iter().for_each(|ev| stats.count(ev));
                stats.batched(batch.len());
                let first_out = pump.handle_batch(&mut batch, &mut Instant::now);
                stats.first_out(first_out);
                stats.tick(&pump.decoder);
            }
        }
    }
    if !pump.render_gone {
        pump.release_if_held();
    }
}

/// Once-a-second debug line on how much the decode thread coalesced.
struct Stats {
    since: Instant,
    batches: u64,
    batched_events: u64,
    coalesced_before: u64,
    /// Longest from the start of a batch to its first result.
    worst_first_out: Duration,
    /// CDF 5/3 passes received this second, as the net thread sent them:
    /// still RLE-coded (`Cdf53Wire`) or expanded (`Cdf53`). A native client
    /// should see only the first; a zero there with the second non-zero means
    /// the flag was lost.
    wire_in: u64,
    planes_in: u64,
    /// Whether the first failed wire expansion was already reported.
    warned_wire_failure: bool,
}

impl Stats {
    fn count(&mut self, ev: &CoreEvent) {
        if let CoreEvent::TilePayload { data, .. } = ev {
            match data {
                TileData::Cdf53Wire { .. } => self.wire_in += 1,
                TileData::Cdf53 { .. } => self.planes_in += 1,
                _ => {}
            }
        }
    }

    fn new() -> Self {
        Stats {
            since: Instant::now(),
            batches: 0,
            batched_events: 0,
            coalesced_before: 0,
            worst_first_out: Duration::ZERO,
            wire_in: 0,
            planes_in: 0,
            warned_wire_failure: false,
        }
    }

    fn first_out(&mut self, d: Option<Duration>) {
        self.worst_first_out = self.worst_first_out.max(d.unwrap_or_default());
    }

    fn batched(&mut self, len: usize) {
        self.batches += 1;
        self.batched_events += len as u64;
    }

    fn tick(&mut self, decoder: &TileDecoder) {
        if Instant::now().saturating_duration_since(self.since) < Duration::from_secs(1) {
            return;
        }
        let coalesced = decoder.cdf53_coalesced();
        let wire_failed = decoder.cdf53_wire_failed();
        if wire_failed > 0 && !self.warned_wire_failure {
            self.warned_wire_failure = true;
            tracing::warn!(
                cdf53_wire_failed = wire_failed,
                "decode thread: a validated CDF 5/3 pass would not expand; dropped"
            );
        }
        if self.batches > 0 || self.wire_in + self.planes_in > 0 || wire_failed > 0 {
            tracing::debug!(
                batches = self.batches,
                batched_events = self.batched_events,
                cdf53_coalesced = coalesced - self.coalesced_before,
                cdf53_coalesced_total = coalesced,
                cdf53_wire_in = self.wire_in,
                cdf53_planes_in = self.planes_in,
                cdf53_wire_failed = wire_failed,
                worst_first_out_us = self.worst_first_out.as_micros() as u64,
                "decode thread: 1s summary"
            );
        }
        *self = Stats {
            since: Instant::now(),
            batches: 0,
            batched_events: 0,
            coalesced_before: coalesced,
            worst_first_out: Duration::ZERO,
            wire_in: 0,
            planes_in: 0,
            warned_wire_failure: self.warned_wire_failure,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_client_gpu::upload_pool::UploadPool;

    /// `UploadPool::with_slabs` makes 1024-slot slabs.
    const SLOTS_PER_SLAB: u32 = 1024;
    use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
    use std::sync::mpsc;

    fn solid(seq: u32) -> CoreEvent {
        CoreEvent::TilePayload {
            frame_seq: seq,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([seq as u8, 0, 0, 255]),
        }
    }

    /// A non-tile event carrying a sequence number in its width.
    fn marker(seq: u32) -> CoreEvent {
        CoreEvent::FrameDimensions {
            width: seq,
            height: 1,
        }
    }

    /// What reached the render channel, as (kind, sequence number).
    #[derive(Debug, PartialEq)]
    enum Seen {
        Slab(u32),
        Vec(u32),
        Marker(u32),
    }

    fn drain(rx: &mpsc::Receiver<RenderMsg>) -> Vec<Seen> {
        let mut seen = Vec::new();
        while let Ok(m) = rx.try_recv() {
            seen.push(match m {
                RenderMsg::SlabTile { frame_seq, .. } => Seen::Slab(frame_seq),
                RenderMsg::Core(CoreEvent::TileReady { frame_seq, .. }) => Seen::Vec(frame_seq),
                RenderMsg::Core(CoreEvent::FrameDimensions { width, .. }) => Seen::Marker(width),
                RenderMsg::Core(other) => panic!("unexpected {other:?}"),
                RenderMsg::Release(_)
                | RenderMsg::DebugMapFrame(..)
                | RenderMsg::H264FramesReady
                | RenderMsg::Redraw
                | RenderMsg::Shutdown => panic!("unexpected message"),
            });
        }
        seen
    }

    /// Slab tiles, `Vec` tiles (after the single slab is spent) and other
    /// events must reach the renderer in the order they were decoded. A bypass
    /// that sends a non-tile event past a held slab tile reorders this list.
    #[test]
    fn slab_vec_and_other_messages_keep_their_order() {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("SKIP: no GPU");
            return;
        };
        let (_pool, alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
        let (tx, rx) = mpsc::channel();
        let (render_tx, render_rx) = mpsc::channel();

        // A marker between every few tiles, across the point where the slab
        // runs out (tile SLOTS_PER_SLAB is the first Vec one).
        let total = SLOTS_PER_SLAB + 40;
        let mut want = Vec::new();
        for seq in 0..total {
            tx.send(solid(seq)).unwrap();
            want.push(if seq < SLOTS_PER_SLAB {
                Seen::Slab(seq)
            } else {
                Seen::Vec(seq)
            });
            if seq % 7 == 3 || seq == SLOTS_PER_SLAB - 1 || seq == SLOTS_PER_SLAB {
                tx.send(marker(seq)).unwrap();
                want.push(Seen::Marker(seq));
            }
        }
        drop(tx);
        run(rx, render_tx, Some(alloc), false);
        assert_eq!(drain(&render_rx), want);
        // The measurement was not vacuous: both kinds occurred.
        assert!(want.iter().any(|s| matches!(s, Seen::Slab(_))));
        assert!(want.iter().any(|s| matches!(s, Seen::Vec(_))));
    }

    /// The outbox releases on count and on age, not only when input runs dry.
    /// The clock is passed in, so nothing here races it.
    #[test]
    fn the_outbox_is_due_by_count_or_age() {
        let msg = || RenderMsg::Core(marker(0));
        let mut o = Outbox::default();
        assert!(!o.due_at(Instant::now()), "empty");
        o.push(msg(), Instant::now());
        let t0 = o.since.expect("set by the first push");
        assert!(!o.due_at(t0), "a fresh message is not due");
        assert!(!o.due_at(t0 + OUTBOX_MAX_AGE / 2));
        assert!(
            o.due_at(t0 + OUTBOX_MAX_AGE),
            "the oldest has waited long enough"
        );
        let (tx, _rx) = mpsc::channel();
        assert!(o.release(None, &tx));
        assert!(o.since.is_none() && o.held.is_empty());
        for _ in 0..OUTBOX_MAX - 1 {
            o.push(msg(), t0);
        }
        let t0 = o.since.unwrap();
        assert!(!o.due_at(t0), "one short of the cap");
        o.push(msg(), t0);
        assert!(o.due_at(t0), "enough messages");
    }

    /// A held batch is released by an event that decodes to nothing once it
    /// is old enough, not only by the next message that produces output.
    #[test]
    fn events_that_decode_to_nothing_still_release_an_aged_batch() {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("SKIP: no GPU");
            return;
        };
        let (_pool, alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
        let (render_tx, render_rx) = mpsc::channel();
        let mut pump = Pump {
            decoder: TileDecoder::new(),
            outbox: Outbox::default(),
            slots: Some(alloc),
            render_tx,
            render_gone: false,
            cdf53_on_gpu: false,
        };
        // A palette update is absorbed by the decoder: no output.
        let nothing = || CoreEvent::PaletteUpdated {
            palette_id: 0,
            colors: vec![[1, 2, 3, 4]],
        };
        let t0 = Instant::now();
        pump.handle(solid(1), t0);
        assert!(drain(&render_rx).is_empty(), "the slab tile is held");
        pump.handle(nothing(), t0);
        assert!(drain(&render_rx).is_empty(), "not old enough yet");
        let held_since = pump.outbox.since.expect("the slab tile is held");
        pump.handle(nothing(), held_since + OUTBOX_MAX_AGE);
        assert_eq!(drain(&render_rx), vec![Seen::Slab(1)], "aged out");
    }

    /// Once the render thread is gone the decoder stops filling slabs: they
    /// would only accumulate write windows nobody closes.
    #[test]
    fn nothing_is_decoded_into_slabs_after_the_render_thread_is_gone() {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("SKIP: no GPU");
            return;
        };
        let (pool, alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
        let (tx, rx) = mpsc::channel();
        let (render_tx, render_rx) = mpsc::channel();
        drop(render_rx);
        for seq in 0..100 {
            tx.send(solid(seq)).unwrap();
        }
        drop(tx);
        run(rx, render_tx, Some(alloc), false);
        // The first release (on the cap, or the age if the machine is slow)
        // fails; every event after it is drained undecoded.
        let issued = pool.stats().slabs[0].0 as usize;
        assert!(0 < issued && issued <= OUTBOX_MAX, "issued {issued}");
        assert!(issued < 100);
    }

    /// Without a pool every tile is a `Vec`, as before slabs existed.
    #[test]
    fn without_slabs_every_tile_is_a_vec() {
        let (tx, rx) = mpsc::channel();
        let (render_tx, render_rx) = mpsc::channel();
        for seq in 0..5 {
            tx.send(solid(seq)).unwrap();
        }
        drop(tx);
        run(rx, render_tx, None, false);
        assert_eq!(drain(&render_rx), (0..5).map(Seen::Vec).collect::<Vec<_>>());
    }

    /// The passes of a real CDF 5/3 tile, round-robined across `tiles`
    /// columns the way a startup burst arrives.
    fn cdf_burst(tiles: u8) -> Vec<CoreEvent> {
        cdf_burst_as(tiles, false)
    }

    /// One CDF 5/3 pass as the core emits it: expanded, or (`wire`) validated
    /// and still RLE-coded, as the native net thread hands it up.
    fn cdf_data(payload: &[u8], idx: u8, new_generation: bool, wire: bool) -> TileData {
        use ghostframe_client_core::cdf53_prevalidate::{prevalidate_cdf53, validate_cdf53};
        if wire {
            let info = validate_cdf53(payload, idx).unwrap();
            TileData::Cdf53Wire {
                pass_idx: idx,
                streams: payload[info.streams].to_vec(),
                present_passes: info.present_passes,
                new_generation,
            }
        } else {
            let pre = prevalidate_cdf53(payload, 0, idx).unwrap();
            TileData::Cdf53 {
                pass_idx: pre.pass_idx,
                bit_planes: pre.bit_planes,
                present_passes: pre.present_passes,
                new_generation,
            }
        }
    }

    fn cdf_burst_as(tiles: u8, wire: bool) -> Vec<CoreEvent> {
        use ghostframe_protocol::codec::cdf53::{encode_passes_sparse, forward};
        let per_tile: Vec<Vec<CoreEvent>> = (0..tiles)
            .map(|x| {
                let tile: Vec<u8> = (0..1024u32)
                    .flat_map(|i| {
                        let (px, py) = ((i % 32) as u8, (i / 32) as u8);
                        [
                            px.wrapping_mul(7).wrapping_add(x.wrapping_mul(23)),
                            py ^ x,
                            px ^ py,
                            255,
                        ]
                    })
                    .collect();
                let (_, passes) = encode_passes_sparse(&forward(&tile));
                passes
                    .into_iter()
                    .enumerate()
                    .map(|(i, (idx, payload))| CoreEvent::TilePayload {
                        frame_seq: i as u32,
                        tile_x: x,
                        tile_y: 0,
                        generation: 0,
                        data: cdf_data(&payload, idx, i == 0, wire),
                    })
                    .collect()
            })
            .collect();
        let longest = per_tile.iter().map(Vec::len).max().unwrap();
        (0..longest)
            .flat_map(|i| per_tile.iter().filter_map(move |t| t.get(i).cloned()))
            .collect()
    }

    /// (tile_x, pixels) of every tile that reached the render channel as a `Vec`.
    fn vec_tiles(rx: &mpsc::Receiver<RenderMsg>) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            match m {
                RenderMsg::Core(CoreEvent::TileReady { tile_x, rgba, .. }) => {
                    out.push((tile_x, rgba))
                }
                other => panic!("unexpected {:?}", std::mem::discriminant(&other)),
            }
        }
        out
    }

    /// With the GPU grid on, a burst of CDF 5/3 passes reaches the render
    /// thread as the passes themselves, every one, in order and undecoded --
    /// no pictures, nothing coalesced -- both when handled one at a time and
    /// as a queued batch; the CPU mode's behaviour is the tests around this.
    #[test]
    fn with_the_gpu_grid_cdf53_passes_are_forwarded_undecoded_in_order() {
        let events = cdf_burst(4);
        let want: Vec<(u8, u8)> = events
            .iter()
            .map(|ev| match ev {
                CoreEvent::TilePayload {
                    tile_x,
                    data: TileData::Cdf53 { pass_idx, .. },
                    ..
                } => (*tile_x, *pass_idx),
                _ => unreachable!(),
            })
            .collect();
        let passes = |rx: &mpsc::Receiver<RenderMsg>| -> Vec<(u8, u8)> {
            let mut out = Vec::new();
            while let Ok(m) = rx.try_recv() {
                match m {
                    RenderMsg::Core(CoreEvent::TilePayload {
                        tile_x,
                        data: TileData::Cdf53 { pass_idx, .. },
                        ..
                    }) => out.push((tile_x, pass_idx)),
                    other => panic!("unexpected {:?}", std::mem::discriminant(&other)),
                }
            }
            out
        };
        for batched in [false, true] {
            let (render_tx, render_rx) = mpsc::channel();
            let mut pump = Pump {
                decoder: TileDecoder::new().with_cdf53_passthrough(true),
                outbox: Outbox::default(),
                slots: None,
                render_tx,
                render_gone: false,
                cdf53_on_gpu: true,
            };
            if batched {
                let mut batch = events.clone();
                pump.handle_batch(&mut batch, &mut Instant::now);
            } else {
                for ev in events.clone() {
                    pump.handle(ev, Instant::now());
                }
            }
            assert_eq!(passes(&render_rx), want, "batched {batched}");
            assert_eq!(pump.decoder.cdf53_coalesced(), 0);
        }
    }

    /// What the net thread now sends -- validated, still RLE-coded passes --
    /// reaches the render thread in GPU mode as exactly the events the
    /// expanded passes did, in order, one at a time and batched: the
    /// renderer, grid and shaders never see the wire variant.
    #[test]
    fn with_the_gpu_grid_wire_passes_are_expanded_and_forwarded_in_order() {
        let planes = cdf_burst_as(4, false);
        let wire = cdf_burst_as(4, true);
        assert!(wire.iter().all(|e| matches!(
            e,
            CoreEvent::TilePayload {
                data: TileData::Cdf53Wire { .. },
                ..
            }
        )));
        for batched in [false, true] {
            let run_one = |events: &[CoreEvent]| -> Vec<CoreEvent> {
                let (render_tx, render_rx) = mpsc::channel();
                let mut pump = Pump {
                    decoder: TileDecoder::new().with_cdf53_passthrough(true),
                    outbox: Outbox::default(),
                    slots: None,
                    render_tx,
                    render_gone: false,
                    cdf53_on_gpu: true,
                };
                if batched {
                    pump.handle_batch(&mut events.to_vec(), &mut Instant::now);
                } else {
                    for ev in events.iter().cloned() {
                        pump.handle(ev, Instant::now());
                    }
                }
                render_rx
                    .try_iter()
                    .map(|m| match m {
                        RenderMsg::Core(ev) => ev,
                        other => panic!("unexpected {:?}", std::mem::discriminant(&other)),
                    })
                    .collect()
            };
            let got = run_one(&wire);
            assert_eq!(got.len(), wire.len(), "batched {batched}: every pass");
            assert_eq!(got, run_one(&planes), "batched {batched}");
        }
    }

    /// CPU mode: wire passes end in the same pictures as expanded ones, with
    /// and without the batching and coalescing of a queued burst.
    #[test]
    fn with_the_cpu_decode_wire_passes_give_the_same_pictures() {
        for bound in [1usize, 64] {
            let pictures = |wire: bool| {
                let (tx, rx) = mpsc::channel();
                let (render_tx, render_rx) = mpsc::channel();
                for ev in cdf_burst_as(4, wire) {
                    tx.send(ev).unwrap();
                }
                drop(tx);
                run_batched(rx, render_tx, None, bound, false);
                vec_tiles(&render_rx)
            };
            let (want, got) = (pictures(false), pictures(true));
            assert!(!want.is_empty());
            assert_eq!(got, want, "bound {bound}");
        }
    }

    /// A burst already queued is decoded together: one picture per tile, the
    /// same pixels the one-at-a-time path ends with.
    #[test]
    fn a_queued_burst_yields_one_picture_per_tile() {
        let events = cdf_burst(4);
        assert!(events.len() > 4 * 3 && events.len() <= BATCH_MAX);

        let (render_tx, render_rx) = mpsc::channel();
        let mut one_by_one = Pump {
            decoder: TileDecoder::new(),
            outbox: Outbox::default(),
            slots: None,
            render_tx,
            render_gone: false,
            cdf53_on_gpu: false,
        };
        for ev in events.iter().cloned() {
            one_by_one.handle(ev, Instant::now());
        }
        let single = vec_tiles(&render_rx);
        assert!(single.len() > 4, "one at a time draws every version");

        let (tx, rx) = mpsc::channel();
        let (render_tx, render_rx) = mpsc::channel();
        for ev in events {
            tx.send(ev).unwrap();
        }
        drop(tx);
        run(rx, render_tx, None, false);
        let batched = vec_tiles(&render_rx);
        let mut xs: Vec<u8> = batched.iter().map(|(x, _)| *x).collect();
        xs.sort();
        assert_eq!(xs, vec![0, 1, 2, 3], "exactly one picture per tile");
        for (x, px) in &batched {
            let last = single.iter().rev().find(|(sx, _)| sx == x).unwrap();
            assert_eq!(&last.1, px, "tile {x}");
        }
    }

    /// More queued than the bound: split into batches, the pictures still end
    /// up the same, and there are fewer of them than passes.
    #[test]
    fn a_burst_longer_than_the_bound_is_split_without_changing_the_result() {
        let events = cdf_burst(10);
        let bound = 64;
        assert!(events.len() > bound);
        let mut dec = TileDecoder::new();
        let mut want = std::collections::HashMap::new();
        let mut one_at_a_time = 0;
        for ev in events.iter().cloned() {
            if let Some(CoreEvent::TileReady { tile_x, rgba, .. }) = dec.decode(ev) {
                want.insert(tile_x, rgba);
                one_at_a_time += 1;
            }
        }
        let (tx, rx) = mpsc::channel();
        let (render_tx, render_rx) = mpsc::channel();
        for ev in events.iter().cloned() {
            tx.send(ev).unwrap();
        }
        drop(tx);
        run_batched(rx, render_tx, None, bound, false);
        let got = vec_tiles(&render_rx);
        // A tile drawn twice means the bound split it across batches (one
        // batch would draw each once); coalescing still beat one at a time.
        // Ignoring the bound fails the first, ignoring coalescing the second.
        assert!(got.len() > 10, "{} pictures", got.len());
        assert!(
            got.len() < one_at_a_time,
            "{} of {one_at_a_time}",
            got.len()
        );
        let last: std::collections::HashMap<_, _> = got.into_iter().collect();
        assert_eq!(last, want);
    }

    /// The same through the upload slabs: a slot per tile, not per pass.
    #[test]
    fn a_queued_burst_takes_one_slab_slot_per_tile() {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("SKIP: no GPU");
            return;
        };
        let (pool, alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
        let (tx, rx) = mpsc::channel();
        let (render_tx, render_rx) = mpsc::channel();
        for ev in cdf_burst(4) {
            tx.send(ev).unwrap();
        }
        drop(tx);
        run(rx, render_tx, Some(alloc), false);
        let slabs = render_rx.try_iter().count();
        assert_eq!(slabs, 4);
        assert_eq!(pool.stats().slabs[0].0, 4, "four slots issued");
    }

    /// A held slab tile is sent while a long batch is still accumulating, not
    /// after it: `Tick` lets the age limit act. The clock is a seam that
    /// jumps 400 us per reading and reports what had reached the render
    /// channel at each, so nothing here races real time.
    #[test]
    fn a_held_slab_tile_is_released_during_a_long_accumulation() {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("SKIP: no GPU");
            return;
        };
        let (_pool, alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
        let (render_tx, render_rx) = mpsc::channel();
        let mut pump = Pump {
            decoder: TileDecoder::new(),
            outbox: Outbox::default(),
            slots: Some(alloc),
            render_tx,
            render_gone: false,
            cdf53_on_gpu: false,
        };
        // A slab tile first, then enough passes to cross several ticks.
        let mut events = vec![CoreEvent::TilePayload {
            frame_seq: 9999,
            tile_x: 250,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([1, 2, 3, 255]),
        }];
        events.extend(cdf_burst(100));
        assert!(events.len() > 3 * 256);
        let mut t = Instant::now();
        let mut seen_at_reading = Vec::new();
        let mut clock = || {
            seen_at_reading.push(render_rx.try_iter().count());
            t += Duration::from_micros(400);
            t
        };
        pump.handle_batch(&mut events, &mut clock);
        assert!(events.is_empty(), "the batch Vec is left empty for reuse");
        // Readings: 0 the batch start, 1 the solid's result (the hold is
        // stamped with it, 800 us in), then one per tick, 400 us apart. The
        // age limit (1 ms) is first met at the third tick (reading 4, 1.2 ms
        // after the stamp), so the tile reaches the channel just after that
        // reading and is counted at reading 5. Not before: the first two
        // ticks are 0.4 and 0.8 ms old. And before any picture, which could
        // only have sent it by the other rule, at the end of the batch.
        let seen: Vec<usize> = seen_at_reading
            .iter()
            .scan(0, |acc, n| {
                *acc += n;
                Some(*acc)
            })
            .collect();
        assert!(seen.len() >= 6, "{seen_at_reading:?}");
        assert_eq!(seen[4], 0, "{seen_at_reading:?}");
        assert!(seen[5] >= 1, "{seen_at_reading:?}");
    }

    /// A held tile is also sent before a reconstruction, never waiting through it.
    #[test]
    fn what_is_held_is_sent_before_each_reconstruction() {
        held_is_sent_before_each_reconstruction(false);
    }

    /// The same with the passes as the net thread sends them (RLE-coded):
    /// they are as heavy as expanded ones in CPU mode.
    #[test]
    fn what_is_held_is_sent_before_each_reconstruction_of_wire_passes() {
        held_is_sent_before_each_reconstruction(true);
    }

    /// `heavy` single-event path: a held slab tile is released before a CPU
    /// reconstruction triggered by a wire pass.
    #[test]
    fn a_wire_pass_is_heavy_in_cpu_mode() {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("SKIP: no GPU");
            return;
        };
        let (_pool, alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
        let (render_tx, render_rx) = mpsc::channel();
        let mut pump = Pump {
            decoder: TileDecoder::new(),
            outbox: Outbox::default(),
            slots: Some(alloc),
            render_tx,
            render_gone: false,
            cdf53_on_gpu: false,
        };
        let t0 = Instant::now();
        // A held slab tile (slab tiles wait for their batch to fill or age).
        pump.handle(
            CoreEvent::TilePayload {
                frame_seq: 9999,
                tile_x: 250,
                tile_y: 0,
                generation: 0,
                data: TileData::Solid([1, 2, 3, 255]),
            },
            t0,
        );
        assert_eq!(render_rx.try_iter().count(), 0, "held, not sent yet");
        // The first pass of a wire tile: heavy, so what is held goes first.
        let first = cdf_burst_as(1, true).remove(0);
        pump.handle(first, t0);
        assert_eq!(
            render_rx.try_iter().count(),
            1,
            "the held tile was released"
        );
    }

    fn held_is_sent_before_each_reconstruction(wire: bool) {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("SKIP: no GPU");
            return;
        };
        let (_pool, alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
        let (render_tx, render_rx) = mpsc::channel();
        let mut pump = Pump {
            decoder: TileDecoder::new(),
            outbox: Outbox::default(),
            slots: Some(alloc),
            render_tx,
            render_gone: false,
            cdf53_on_gpu: false,
        };
        let mut events = cdf_burst_as(3, wire);
        // A Solid for another tile, after the passes: decoded in place
        // (held), then the end-of-batch flush has three pictures to make.
        events.push(CoreEvent::TilePayload {
            frame_seq: 9999,
            tile_x: 250,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([1, 2, 3, 255]),
        });
        let mut t = Instant::now();
        let mut log = Vec::new();
        let mut clock = || {
            log.push(render_rx.try_iter().count());
            // Never due by age, so only the pre-reconstruction rule acts.
            t += Duration::from_nanos(1);
            t
        };
        pump.handle_batch(&mut events, &mut clock);
        // Readings: start, then one per result (solid, 3 pictures), then the
        // end. The solid is held at its own reading and released before the
        // first reconstruction, so the first picture's reading sees it.
        assert_eq!(log.len(), 6, "{log:?}");
        assert_eq!(log[1], 0);
        assert_eq!(log[2], 1, "the solid was sent before the picture: {log:?}");
    }

    // ---- measurement ------------------------------------------------------

    /// CPU time of the calling thread, in nanoseconds.
    fn thread_cpu_ns() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid out-pointer for the call's duration.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        assert_eq!(rc, 0);
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    /// A start-up burst for a `cols` x `rows` screen: pass 0 of every tile,
    /// then pass 1 of every tile, and so on, from 24 distinct pictures.
    fn screen_burst(cols: u8, rows: u8, wire: bool) -> Vec<CoreEvent> {
        use ghostframe_protocol::codec::cdf53::{encode_passes_sparse, forward};
        let pictures: Vec<Vec<_>> = (0..24u32)
            .map(|id| {
                let tile: Vec<u8> = (0..1024u32)
                    .flat_map(|i| {
                        let (px, py) = (i % 32, i / 32);
                        let text = if (px / 3 + py / 5 + id) % 3 == 0 {
                            0x20
                        } else {
                            0xE0
                        };
                        let ramp = (px * 5 + py * 3 + id * 11) as u8;
                        let (b, g, r) = match id % 3 {
                            0 => (text, text, text),
                            1 => (ramp, ramp.wrapping_add(40), 255 - ramp),
                            _ => (ramp ^ (py as u8 * 9), text, (px as u8) * 6),
                        };
                        [b, g, r, 255]
                    })
                    .collect();
                let (_, passes) = encode_passes_sparse(&forward(&tile));
                passes.into_iter().collect()
            })
            .collect();
        let mut events = Vec::new();
        for layer in 0..14usize {
            for ty in 0..rows {
                for tx in 0..cols {
                    let pic = &pictures[(tx as usize + ty as usize * 7) % pictures.len()];
                    // Passes the picture does not have (sparse) are not sent.
                    let Some((idx, payload)) = pic.iter().find(|(i, _)| *i as usize == layer)
                    else {
                        continue;
                    };
                    events.push(CoreEvent::TilePayload {
                        frame_seq: layer as u32,
                        tile_x: tx,
                        tile_y: ty,
                        generation: 0,
                        data: cdf_data(payload, *idx, layer == 0, wire),
                    });
                }
            }
        }
        events
    }

    /// Decode-thread and render-thread CPU for a start-up burst, with CDF 5/3
    /// decoded on the CPU (decode thread) and on the GPU (grid, render
    /// thread): `cargo test -p ghostframe-client-native --no-default-features
    /// --features gles,decode-gstreamer --release --lib -- --ignored
    /// --nocapture cdf53_mode_cost`. Also checks that both end with the same
    /// picture.
    #[test]
    #[ignore = "measurement, prints numbers"]
    fn cdf53_mode_cost() {
        use ghostframe_client_gpu::renderer::Renderer;
        let ctx = WgpuContext::new().expect("wgpu context");
        let (cols, rows) = (60u8, 34u8); // 1920 x 1080
        let plane_events = screen_burst(cols, rows, false);
        let wire_events = screen_burst(cols, rows, true);
        let events = &plane_events;
        let tiles = cols as usize * rows as usize;
        println!("{} events for {tiles} tiles", events.len());
        let mut pictures: Vec<Vec<u8>> = Vec::new();
        // `wire`: the passes arrive RLE-coded (what the net thread now hands
        // up) and the decode thread expands them, which is the cost moved off
        // gf-net.
        for (gpu, wire) in [
            (false, false),
            (true, false),
            (false, true),
            (true, true),
            (false, false),
            (true, false),
            (false, true),
            (true, true),
        ] {
            let mut r = Renderer::new(
                &ctx,
                cols as u32 * 32,
                rows as u32 * 32,
                3,
                &[],
                false,
                false,
            )
            .expect("renderer");
            let (pool, alloc) = UploadPool::new(&ctx).expect("pool");
            r.attach_upload_pool(&ctx, &pool);
            if gpu {
                r.enable_cdf53_gpu(&ctx);
            }
            let (tx, rx) = mpsc::channel();
            let (render_tx, render_rx) = mpsc::channel();
            for ev in if wire { &wire_events } else { events }.iter().cloned() {
                tx.send(ev).unwrap();
            }
            drop(tx);
            let wall = Instant::now();
            let decode = std::thread::spawn(move || {
                let c0 = thread_cpu_ns();
                run(rx, render_tx, Some(alloc), gpu);
                thread_cpu_ns() - c0
            });
            let c0 = thread_cpu_ns();
            let (mut flushes, mut msgs) = (0u32, 0u64);
            let mut open = true;
            while open || r.has_deferred_work() {
                let mut got = 0;
                if open {
                    // Block for the first message, then take what is queued,
                    // as the render loop does.
                    match render_rx.recv() {
                        Ok(m) => {
                            apply(&mut r, &ctx, m);
                            got += 1;
                        }
                        Err(_) => open = false,
                    }
                    while open && got < 2048 {
                        match render_rx.try_recv() {
                            Ok(m) => {
                                apply(&mut r, &ctx, m);
                                got += 1;
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => open = false,
                        }
                    }
                }
                msgs += got;
                r.flush(&ctx).expect("flush");
                flushes += 1;
            }
            let render_cpu = thread_cpu_ns() - c0;
            let decode_cpu = decode.join().unwrap();
            // The read-back waits for the GPU, so the wall time includes it.
            let picture = r.debug_read_framebuffer(&ctx);
            #[allow(clippy::disallowed_methods, reason = "measuring real elapsed time")]
            let wall_ms = wall.elapsed().as_secs_f64() * 1e3;
            println!(
                "{} {} : decode thread {:7.1} ms CPU, render thread {:7.1} ms CPU, wall {:7.1} ms, {flushes} flushes, {msgs} msgs",
                if gpu { "gpu" } else { "cpu" },
                if wire { "wire  " } else { "planes" },
                decode_cpu as f64 / 1e6,
                render_cpu as f64 / 1e6,
                wall_ms
            );
            pictures.push(picture);
        }
        assert!(
            pictures.iter().all(|p| p == &pictures[0]),
            "the two modes ended with different pictures"
        );
        let drawn = pictures[0]
            .chunks(4)
            .filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
            .count();
        assert!(drawn > 1_000_000, "only {drawn} non-black pixels");
    }

    fn apply(r: &mut ghostframe_client_gpu::renderer::Renderer, ctx: &WgpuContext, m: RenderMsg) {
        match m {
            RenderMsg::Core(ev) => r.apply_event(ctx, ev).expect("apply"),
            RenderMsg::SlabTile {
                frame_seq,
                tile_x,
                tile_y,
                slot,
            } => assert!(r.apply_slot_tile(frame_seq, tile_x, tile_y, slot)),
            _ => panic!("unexpected message"),
        }
    }
}
