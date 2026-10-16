//! Server send-path dispatch cost: scheduler tick -> fragment_tile ->
//! `submit_batch` (stamp, retransmit cache, RTO, FEC) -> `drain` into a sender
//! that does what `WebTransport::send_datagram` does to each datagram.
//!
//! Not a Criterion bench: the numbers that matter here are the median and the
//! spread of one *whole-screen* dispatch, plus how many heap allocations and
//! bytes it costs, and Criterion's per-iteration setup handling would hide
//! the allocation counts. Run:
//!
//! ```text
//! cargo bench -p ghostframe-bench --bench send_path
//! ```
//!
//! Env: `SEND_PATH_ITERS` (default 40), `SEND_PATH_JSON=1` for one JSON line
//! per scenario.
//!
//! The sender copies every datagram behind a 1-byte varint prefix into a new
//! `Bytes`, exactly the work `WebTransport::send_datagram` does before
//! `conn.datagrams().send`; that is the stage this bench can reach without a
//! live quinn connection. The tsnet framing after quinn's `poll_transmit` is
//! not covered.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use ghostframe_lib::transport::protocol::{fragment_tile, Codec, TileFragmentInputs};
use ghostframe_lib::transport::reliable_emitter::traits::{DatagramSender, SendOutcome};
use ghostframe_lib::transport::reliable_emitter::{EmitKey, ReliableTileEmitter};
use ghostframe_lib::transport::scheduler::{Scheduler, TileWork, WorkState};

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(n as u64, Ordering::Relaxed);
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static A: Counting = Counting;

fn counters() -> (u64, u64) {
    (
        ALLOCS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    )
}

/// What `WebTransport::send_datagram` does to a datagram.
struct WtLikeSender {
    sent: Vec<Bytes>,
    wire_bytes: usize,
}
impl DatagramSender for WtLikeSender {
    fn send(&mut self, dg: &[u8]) -> SendOutcome {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0x00]);
        buf.extend_from_slice(dg);
        self.wire_bytes += buf.len();
        self.sent.push(Bytes::from(buf));
        SendOutcome::Sent
    }
}

const MAX_FRAG: usize = 1150;

struct Stage {
    tick: Duration,
    fragment: Duration,
    submit: Duration,
    drain: Duration,
    allocs: u64,
    bytes: u64,
    datagrams: usize,
}

fn one_frame(cols: u32, rows: u32, payload_len: usize) -> Stage {
    let now = Instant::now();
    let mut sched = Scheduler::new(cols, rows);
    let payload: Vec<u8> = (0..payload_len).map(|i| (i * 31 + 7) as u8).collect();
    for ty in 0..rows {
        for tx in 0..cols {
            sched.enqueue_at(
                TileWork {
                    tile_x: tx as u8,
                    tile_y: ty as u8,
                    generation: 1,
                    pass_idx: 0,
                    total_passes: 1,
                    codec: Codec::Cdf53,
                    payload: payload.clone(),
                    queued_at: now,
                    last_sent_at: None,
                    state: WorkState::Pending,
                },
                now,
            );
        }
    }
    let mut emitter = ReliableTileEmitter::new(now);
    let mut sender = WtLikeSender {
        sent: Vec::with_capacity(cols as usize * rows as usize * 2),
        wire_bytes: 0,
    };
    let (a0, b0) = counters();

    let t0 = Instant::now();
    let drained = sched.tick_at_with(usize::MAX, now, &|_, _, _| false);
    let t1 = Instant::now();
    let seq = 7u32 | 0x8000_0000;
    let mut items = Vec::with_capacity(drained.len());
    for work in drained {
        let dgs = fragment_tile(
            &TileFragmentInputs {
                frame_seq: seq,
                tile_x: work.tile_x,
                tile_y: work.tile_y,
                codec: work.codec,
                generation: work.generation,
                pass: work.pass_idx,
                timestamp_us: 0,
            },
            &work.payload,
            MAX_FRAG,
        );
        let key = EmitKey::new(seq, work.tile_x, work.tile_y, work.pass_idx);
        for dg in dgs {
            items.push((key, Bytes::from(dg), work.queued_at, None));
        }
    }
    let t2 = Instant::now();
    emitter.submit_batch(items, now);
    let t3 = Instant::now();
    emitter.drain(&mut sender, now);
    let t4 = Instant::now();
    let (a1, b1) = counters();
    let datagrams = sender.sent.len();
    std::hint::black_box(&sender);
    Stage {
        tick: t1 - t0,
        fragment: t2 - t1,
        submit: t3 - t2,
        drain: t4 - t3,
        allocs: a1 - a0,
        bytes: b1 - b0,
        datagrams,
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let iters: usize = std::env::var("SEND_PATH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let json = std::env::var("SEND_PATH_JSON").is_ok();
    // (label, tile cols, tile rows). 32 px tiles.
    let screens = [
        ("1080p", 60u32, 34u32),
        ("4K", 120, 68),
        ("dual-4K", 240, 68),
    ];
    let payloads = [300usize, 900, 4000];
    println!(
        "{:<8} {:>6} {:>7} | {:>8} {:>8} {:>8} {:>8} | {:>8} {:>8} {:>8} | {:>9} {:>11}",
        "screen",
        "payld",
        "dgrams",
        "tick",
        "frag",
        "submit",
        "drain",
        "total",
        "min",
        "max",
        "allocs/dg",
        "bytes/dg"
    );
    for (label, cols, rows) in screens {
        for &p in &payloads {
            // Warm-up.
            let _ = one_frame(cols, rows, p);
            let mut tot = Vec::new();
            let (mut tk, mut fr, mut su, mut dr) = (vec![], vec![], vec![], vec![]);
            let mut last = None;
            for _ in 0..iters {
                let s = one_frame(cols, rows, p);
                let ms = |d: Duration| d.as_secs_f64() * 1e3;
                tk.push(ms(s.tick));
                fr.push(ms(s.fragment));
                su.push(ms(s.submit));
                dr.push(ms(s.drain));
                tot.push(ms(s.tick + s.fragment + s.submit + s.drain));
                last = Some(s);
            }
            let s = last.unwrap();
            let (mn, mx) = (
                tot.iter().cloned().fold(f64::MAX, f64::min),
                tot.iter().cloned().fold(0.0, f64::max),
            );
            let (mtk, mfr, msu, mdr, mto) = (
                median(&mut tk),
                median(&mut fr),
                median(&mut su),
                median(&mut dr),
                median(&mut tot),
            );
            println!(
                "{:<8} {:>6} {:>7} | {:>8.3} {:>8.3} {:>8.3} {:>8.3} | {:>8.3} {:>8.3} {:>8.3} | {:>9.2} {:>11.0}",
                label, p, s.datagrams, mtk, mfr, msu, mdr, mto, mn, mx,
                s.allocs as f64 / s.datagrams as f64,
                s.bytes as f64 / s.datagrams as f64
            );
            if json {
                println!(
                    "{{\"screen\":\"{label}\",\"payload\":{p},\"datagrams\":{},\"median_ms\":{mto:.4},\"min_ms\":{mn:.4},\"max_ms\":{mx:.4},\"allocs\":{},\"bytes\":{}}}",
                    s.datagrams, s.allocs, s.bytes
                );
            }
        }
    }
}
