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

use ghostframe_client_core::{Decoded, Event as CoreEvent, TileData, TileDecoder};
use ghostframe_client_gpu::upload_pool::{SlotAllocator, SlotRef};

use crate::render_thread::RenderMsg;

/// Most messages held back before they are sent regardless. See [`Outbox`].
const OUTBOX_MAX: usize = 32;

/// Longest the oldest held message waits while input keeps arriving. A CDF 5/3
/// tile can take several milliseconds to decode on this hardware, so the count
/// alone does not bound the delay behind the first slab tile.
const OUTBOX_MAX_AGE: Duration = Duration::from_millis(1);

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
    fn release(&mut self, slots: &mut Option<SlotAllocator>, tx: &Sender<RenderMsg>) -> bool {
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

impl Pump {
    /// Release what is held; remember if the render thread is gone.
    fn release(&mut self) {
        if !self.outbox.release(&mut self.slots, &self.render_tx) {
            // Keep draining so the net thread never blocks or errors on us;
            // its ACK/NACK timing must keep running until it is told to stop.
            tracing::warn!("render thread channel closed; dropping decoded events");
            self.render_gone = true;
        }
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
        let msg = match decoded {
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
        };
        let waits = !self.outbox.held.is_empty() || matches!(msg, RenderMsg::SlabTile { .. });
        self.outbox.push(msg);
        if !waits || self.outbox.due_at(now) {
            self.release();
        }
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
    let mut pump = Pump {
        decoder: TileDecoder::new(),
        outbox: Outbox::default(),
        slots,
        render_tx,
        render_gone: false,
    };
    loop {
        let ev = match rx.try_recv() {
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
        pump.handle(ev, Instant::now());
    }
    if !pump.render_gone {
        pump.release_if_held();
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
        assert!(o.release(&mut None, &tx));
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
}
