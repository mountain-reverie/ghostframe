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
    fn push(&mut self, msg: RenderMsg) {
        self.since.get_or_insert_with(Instant::now);
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
    outbox.push(msg);
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
        let heavy = matches!(
            &ev,
            CoreEvent::TilePayload {
                data: TileData::Cdf53 { .. },
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
        let heavy = events.iter().any(|ev| {
            matches!(
                ev,
                CoreEvent::TilePayload {
                    data: TileData::Cdf53 { .. },
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
pub(crate) fn run(
    rx: Receiver<CoreEvent>,
    render_tx: Sender<RenderMsg>,
    slots: Option<SlotAllocator>,
) {
    run_batched(rx, render_tx, slots, BATCH_MAX)
}

/// [`run`] with the batch bound chosen, so that a test can cross it cheaply.
fn run_batched(
    rx: Receiver<CoreEvent>,
    render_tx: Sender<RenderMsg>,
    slots: Option<SlotAllocator>,
    batch_max: usize,
) {
    let mut pump = Pump {
        decoder: TileDecoder::new(),
        outbox: Outbox::default(),
        slots,
        render_tx,
        render_gone: false,
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
            Err(TryRecvError::Empty) => pump.handle(first, Instant::now()),
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
}

impl Stats {
    fn new() -> Self {
        Stats {
            since: Instant::now(),
            batches: 0,
            batched_events: 0,
            coalesced_before: 0,
            worst_first_out: Duration::ZERO,
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
        if self.batches > 0 {
            tracing::debug!(
                batches = self.batches,
                batched_events = self.batched_events,
                cdf53_coalesced = coalesced - self.coalesced_before,
                cdf53_coalesced_total = coalesced,
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
        run(rx, render_tx, Some(alloc));
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
        o.push(msg());
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
            o.push(msg());
        }
        let t0 = o.since.unwrap();
        assert!(!o.due_at(t0), "one short of the cap");
        o.push(msg());
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
        run(rx, render_tx, Some(alloc));
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
        run(rx, render_tx, None);
        assert_eq!(drain(&render_rx), (0..5).map(Seen::Vec).collect::<Vec<_>>());
    }

    /// The passes of a real CDF 5/3 tile, round-robined across `tiles`
    /// columns the way a startup burst arrives.
    fn cdf_burst(tiles: u8) -> Vec<CoreEvent> {
        use ghostframe_client_core::cdf53_prevalidate::prevalidate_cdf53;
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
                    .map(|(i, (idx, payload))| {
                        let pre = prevalidate_cdf53(&payload, 0, idx).unwrap();
                        CoreEvent::TilePayload {
                            frame_seq: i as u32,
                            tile_x: x,
                            tile_y: 0,
                            generation: 0,
                            data: TileData::Cdf53 {
                                pass_idx: pre.pass_idx,
                                bit_planes: pre.bit_planes,
                                present_passes: pre.present_passes,
                                new_generation: i == 0,
                            },
                        }
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
        run(rx, render_tx, None);
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
        run_batched(rx, render_tx, None, bound);
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
        run(rx, render_tx, Some(alloc));
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
        // Readings: the batch start, the solid's result, then one per tick
        // (400 us apart, so the solid becomes overdue at the first or second
        // tick). It is not sent by its own reading or the first tick's, and
        // is by the third tick: before any picture, which could only have
        // sent it by the other rule, at the first reading after the last pass.
        let seen: Vec<usize> = seen_at_reading
            .iter()
            .scan(0, |acc, n| {
                *acc += n;
                Some(*acc)
            })
            .collect();
        assert!(seen.len() >= 5, "{seen_at_reading:?}");
        assert_eq!(seen[2], 0, "{seen_at_reading:?}");
        assert!(seen[4] >= 1, "{seen_at_reading:?}");
    }

    /// A held tile is also sent before a reconstruction, never waiting through it.
    #[test]
    fn what_is_held_is_sent_before_each_reconstruction() {
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
        };
        let mut events = cdf_burst(3);
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
}
