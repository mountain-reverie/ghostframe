//! The `Content::Unchanged` oracle (damage-driven encoding PR 7, library
//! level): the same scripted frame sequence and the same scripted ACKs go
//! through the bridge twice, once with every tick carrying its pixels
//! (full-compare mode) and once with the ticks nothing changed on sent as
//! `Unchanged` (damage mode). The scheduler and `metrics_tracker` state is
//! compared after every tick: PixelPerfect transitions, idle escalation,
//! generations, the queues, the escalation candidates the loop asked for.
//!
//! The backend is a stand-in that behaves as the GPU path does where it
//! matters here: the SAD reference (`prev`) and the upload image (`upload`)
//! are separate, the compare is thresholded so they can differ, an unchanged
//! tick never sees pixels, and the escalation forward reads `upload`, never
//! `prev` (design 3.3, H2).

use super::*;
use crate::capture::gpu_pipeline::TileAnalysis;
use crate::frame_damage::Content;
use crate::tile::{CodecState, TILE_SIZE};
use std::sync::{Arc, Mutex};

const COLS: u32 = 4;
const ROWS: u32 = 3;
const W: u32 = COLS * TILE_SIZE;
const H: u32 = ROWS * TILE_SIZE;
/// The real compare's threshold: a tile is dirty above this SAD.
const SAD_THRESHOLD: u64 = 64;
const TICKS: u32 = 150;

/// Wait for the worker's next message (it runs on its own thread).
async fn next_worker_message(bridge: &mut IoBridge) -> Option<FromWorker> {
    let rx = &mut bridge.frame_pipeline.as_mut().unwrap().rx;
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the worker answered within 5 s")
}

/// Every tile's pixels as a pure function of `(tile, version)`.
fn tile_bytes(tile: u32, version: u32) -> Vec<u8> {
    let mut x = u64::from(tile) * 0x9E37_79B9 + u64::from(version) * 0x85EB_CA6B + 1;
    (0..TILE_SIZE * TILE_SIZE * 4)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// The scripted screen at `tick`: which version each tile shows, and a few
/// pixels nudged under the threshold on the drifting tile.
fn screen(tick: u32) -> Vec<u8> {
    let mut v = vec![1u32; (COLS * ROWS) as usize];
    if tick >= 5 {
        v[5] = 2;
    }
    if tick >= 40 {
        v[2] = 2;
    }
    if tick >= 90 {
        v[5] = 3;
    }
    let mut px = vec![0u8; (W * H * 4) as usize];
    let grid = crate::tile::TileGrid::new(W, H);
    for ty in 0..ROWS {
        for tx in 0..COLS {
            let t = tile_bytes(ty * COLS + tx, v[(ty * COLS + tx) as usize]);
            for row in 0..TILE_SIZE {
                let dst = (((ty * TILE_SIZE + row) * W + tx * TILE_SIZE) * 4) as usize;
                let src = (row * TILE_SIZE * 4) as usize;
                px[dst..dst + (TILE_SIZE * 4) as usize]
                    .copy_from_slice(&t[src..src + (TILE_SIZE * 4) as usize]);
            }
        }
    }
    let _ = grid;
    // Sub-threshold drift on tile 11: a few bytes by 1, from tick 20 on.
    if tick >= 20 {
        let base = (((2 * TILE_SIZE) * W + 3 * TILE_SIZE) * 4) as usize;
        for k in 0..8 {
            px[base + k * 4] = px[base + k * 4].wrapping_add(1);
        }
    }
    px
}

/// Whether `tick` carries a change the producer would report.
fn changes_at(tick: u32) -> bool {
    matches!(tick, 0 | 5 | 20 | 40 | 90)
}

/// What the stand-in did, for the comparison.
#[derive(Default)]
struct Seen {
    /// The escalation candidates each `analyse` was given.
    escalation: Vec<Vec<u32>>,
    /// `(tick, tile)` for every escalation forward the finish encoded, with
    /// a hash of the pixels it read.
    forwarded: Vec<(usize, u32, u64)>,
}

struct ScreenBackend {
    /// The SAD reference.
    prev: Option<Vec<u8>>,
    /// The persistent upload image.
    upload: Option<Vec<u8>>,
    dirty: Vec<u32>,
    seen: Arc<Mutex<Seen>>,
}

fn sad(a: &[u8], b: &[u8], tile: u32) -> u64 {
    let (tx, ty) = (tile % COLS, tile / COLS);
    let mut s = 0u64;
    for row in 0..TILE_SIZE {
        let o = (((ty * TILE_SIZE + row) * W + tx * TILE_SIZE) * 4) as usize;
        for i in 0..(TILE_SIZE * 4) as usize {
            s += u64::from(a[o + i].abs_diff(b[o + i]));
        }
    }
    s
}

fn hash(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| {
        (h ^ u64::from(x)).wrapping_mul(0x100_0000_01b3)
    })
}

fn encode(pixels: &[u8], x: u32, y: u32) -> Cdf53Encoded {
    let grid = crate::tile::TileGrid::new(W, H);
    let tile = grid.extract_tile(pixels, W * 4, x, y);
    let coeffs = crate::encoder::cdf53::forward(&tile);
    let (present_passes, passes) = crate::encoder::cdf53::encode_passes_sparse(&coeffs);
    Cdf53Encoded {
        present_passes,
        passes,
    }
}

impl FrameBackend for ScreenBackend {
    fn apply(&mut self, _control: Control) {}

    fn analyse(
        &mut self,
        frame: &FrameSubmission,
        escalation: &[u32],
        _want_diag: bool,
    ) -> Result<Analysed, AnalyseError> {
        self.seen
            .lock()
            .unwrap()
            .escalation
            .push(escalation.to_vec());
        match frame.content {
            Content::Pixels => self.upload = Some(frame.pixels.to_vec()),
            Content::Unchanged => {
                assert!(
                    frame.pixels.is_empty(),
                    "an unchanged tick carries no pixels"
                );
                if self.upload.is_none() {
                    return Err(String::from("no upload image").into());
                }
            }
        }
        let up = self.upload.as_ref().unwrap();
        self.dirty = match &self.prev {
            None => (0..COLS * ROWS).collect(),
            Some(prev) => (0..COLS * ROWS)
                .filter(|&t| sad(prev, up, t) > SAD_THRESHOLD)
                .collect(),
        };
        // Commit the dirty tiles into the reference, as the GPU path does.
        let mut next = self.prev.take().unwrap_or_else(|| up.clone());
        for &t in &self.dirty {
            let (tx, ty) = (t % COLS, t / COLS);
            for row in 0..TILE_SIZE {
                let o = (((ty * TILE_SIZE + row) * W + tx * TILE_SIZE) * 4) as usize;
                next[o..o + (TILE_SIZE * 4) as usize]
                    .copy_from_slice(&up[o..o + (TILE_SIZE * 4) as usize]);
            }
        }
        self.prev = Some(next);
        let rich = TileAnalysis {
            count: 200,
            edge_density_thou: 400,
            ..Default::default()
        };
        Ok(Analysed {
            dirty_analysis: vec![Some(rich); self.dirty.len()],
            dirty_tiles: self.dirty.clone(),
            ..Default::default()
        })
    }

    fn finish_h264(&mut self, _f: &FrameSubmission, _k: bool) -> Option<H264Out> {
        None
    }

    fn finish_tile(&mut self, frame: &FrameSubmission, req: &TileRequest<'_>) -> TileOut {
        let up = self.upload.clone().unwrap();
        let mut seen = self.seen.lock().unwrap();
        let tick = seen.escalation.len();
        // Dirty tiles the classifier left on the CDF 5/3 route come from the
        // GPU's compact list; the rest the loop sent as `fallback`.
        let cdf53_dirty = self
            .dirty
            .iter()
            .filter(|&&t| !req.fallback.contains(&(t % COLS, t / COLS)))
            .map(|&t| Cdf53Tile {
                tile_x: (t % COLS) as u8,
                tile_y: (t / COLS) as u8,
                encoded: encode(&up, t % COLS, t / COLS),
            })
            .collect();
        let cdf53_escalation = req
            .escalation
            .iter()
            .map(|slot| {
                let (x, y) = (slot.tile_idx % COLS, slot.tile_idx / COLS);
                let grid = crate::tile::TileGrid::new(W, H);
                // What the forward read: the upload image, never `prev`.
                let h = hash(&grid.extract_tile(&up, W * 4, x, y));
                seen.forwarded.push((tick, slot.tile_idx, h));
                Some(encode(&up, x, y))
            })
            .collect();
        let cdf53_fallback = req
            .fallback
            .iter()
            .map(|&(x, y)| ((x, y), encode(&up, x, y)))
            .collect();
        TileOut {
            pixels: Ok(if frame.content == Content::Unchanged {
                PixelSource::Owned(Vec::new())
            } else {
                PixelSource::Frame
            }),
            palrle: OwnedPalRle::default(),
            cdf53_dirty,
            cdf53_escalation,
            cdf53_fallback,
            timings: TileTimings::default(),
        }
    }

    fn finish_discard(&mut self) {}

    fn forward_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<i16>, String> {
        Ok(tiles
            .iter()
            .flat_map(|t| crate::encoder::cdf53::forward(t))
            .collect())
    }
}

/// The comparable state of the bridge after one tick.
#[derive(Debug, PartialEq)]
struct Snap {
    tiles: Vec<String>,
    generations: Vec<u8>,
    acked: Vec<u8>,
    mode: String,
    queue_len: usize,
    queue: String,
    refinement: String,
    escalation_asked: Vec<u32>,
    forwarded: Vec<(u32, u64)>,
}

/// The queued work without its timestamps (they come from the wall clock).
fn work_summary(work: Vec<crate::transport::scheduler::TileWork>) -> String {
    work.iter()
        .map(|w| {
            format!(
                "{},{} g{} p{} {:?} {}b",
                w.tile_x,
                w.tile_y,
                w.generation,
                w.pass_idx,
                w.state,
                w.payload.len()
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn snapshot(b: &IoBridge, seen: &Seen, tick: usize) -> Snap {
    let n = (COLS * ROWS) as usize;
    Snap {
        tiles: (0..n as u32)
            .map(|i| format!("{:?}", b.metrics_tracker.get(i % COLS, i / COLS)))
            .collect(),
        generations: (0..n as u32)
            .map(|i| {
                b.scheduler
                    .generation_for((i % COLS) as u8, (i / COLS) as u8)
            })
            .collect(),
        acked: (0..n as u32)
            .map(|i| {
                let (x, y) = ((i % COLS) as u8, (i / COLS) as u8);
                b.scheduler
                    .cdf53_passes_acked_count(x, y, b.scheduler.generation_for(x, y))
            })
            .collect(),
        mode: format!("{:?}", b.frame_mode),
        queue_len: b.scheduler.queue_len(),
        queue: work_summary(b.scheduler.peek_for_test()),
        refinement: work_summary(b.scheduler.refinement_peek_for_test()),
        escalation_asked: seen.escalation.get(tick).cloned().unwrap_or_default(),
        forwarded: seen
            .forwarded
            .iter()
            .filter(|f| f.0 == tick + 1)
            .map(|f| (f.1, f.2))
            .collect(),
    }
}

/// What a run ended with, besides the per-tick states.
struct Run {
    snaps: Vec<Snap>,
    unchanged_ticks: u32,
    pixel_perfect_tiles: usize,
    escalated: usize,
    forwarded: Vec<(usize, u32, u64)>,
}

/// Every escalation forward read what the screen showed at that tick, not
/// what the SAD reference holds (they differ on the drifting tile).
fn forwards_read_the_screen(run: &Run, forwards: &[(usize, u32, u64)]) -> Result<(), String> {
    let _ = run;
    for &(tick1, tile, h) in forwards {
        let px = screen((tick1 - 1) as u32);
        let grid = crate::tile::TileGrid::new(W, H);
        let want = hash(&grid.extract_tile(&px, W * 4, tile % COLS, tile / COLS));
        if h != want {
            return Err(format!(
                "tick {}: the forward of tile {tile} read stale pixels",
                tick1 - 1
            ));
        }
    }
    Ok(())
}

/// One pass over the script. `damage_mode` sends `Unchanged` for the ticks
/// the script says nothing changed on.
async fn run(damage_mode: bool) -> Run {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let backend = ScreenBackend {
        prev: None,
        upload: None,
        dirty: Vec::new(),
        seen: seen.clone(),
    };
    let (our_end, peer) = std::os::unix::net::UnixStream::pair().expect("pair");
    our_end.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(
        tokio::net::UnixStream::from_std(our_end).unwrap(),
        server,
        rx,
    );
    bridge.set_frame_backend(Box::new(backend));
    bridge.test_commit_dg_size = Some(1200);
    let _peer = tokio::net::UnixStream::from_std(peer).unwrap();

    let mut snaps = Vec::new();
    let mut unchanged_ticks = 0;
    for tick in 0..TICKS {
        // A few tiles go back to the full-frame encoder mid-run, so the
        // idle-escalation counters have something to count.
        if tick == 45 {
            for t in [0u32, 1, 4, 11] {
                bridge
                    .metrics_tracker
                    .get_mut(t % COLS, t / COLS)
                    .codec_state = CodecState::H264 { frames_in_h264: 40 };
            }
        }
        let unchanged = damage_mode && !changes_at(tick);
        let frame = FrameSubmission {
            width: W,
            height: H,
            stride: W * 4,
            pixels: if unchanged {
                Default::default()
            } else {
                screen(tick).into()
            },
            content: if unchanged {
                Content::Unchanged
            } else {
                Content::Pixels
            },
            dmabuf_fd: None,
            timestamp_us: tick * 33_333,
            damage: crate::frame_damage::FrameDamage::Unknown,
            capture_done_ns: 0,
        };
        unchanged_ticks += u32::from(unchanged);
        bridge.frame_seq += 1;
        let seq = bridge.frame_seq;
        let candidates = bridge.collect_escalation_candidates();
        bridge.start_analysis(frame, seq, 1200, candidates, 0);
        // Analysed, then the finish.
        for _ in 0..2 {
            let msg = next_worker_message(&mut bridge).await.expect("worker");
            bridge.on_frame_worker_message(msg);
            if bridge.in_flight.is_none() {
                break;
            }
        }
        assert!(
            bridge.in_flight.is_none(),
            "tick {tick}: the frame finished"
        );

        // Scripted ACKs (and so scripted loss): a present pass of a CDF 5/3
        // tile is acknowledged on the ticks where the pattern says so, and
        // is "lost" in between. (No session is connected, so the loop's own
        // emission goes nowhere; the ACKs are applied to the scheduler as the
        // ACK dispatcher would.)
        for i in 0..COLS * ROWS {
            let (x, y) = ((i % COLS) as u8, (i / COLS) as u8);
            let CodecState::Cdf53 { present_passes, .. } =
                bridge.metrics_tracker.get(i % COLS, i / COLS).codec_state
            else {
                continue;
            };
            let gen = bridge.scheduler.generation_for(x, y);
            for p in (0..16u8).filter(|p| present_passes & (1 << p) != 0) {
                if (tick as usize * 5 + i as usize * 3 + usize::from(p) * 2).is_multiple_of(11) {
                    bridge.scheduler.record_cdf53_ack(x, y, gen, p);
                }
            }
        }
        snaps.push(snapshot(&bridge, &seen.lock().unwrap(), tick as usize));
    }
    let pixel_perfect_tiles = (0..COLS * ROWS)
        .filter(|&i| {
            matches!(
                bridge.metrics_tracker.get(i % COLS, i / COLS).codec_state,
                CodecState::PixelPerfect
            )
        })
        .count();
    let escalated = seen
        .lock()
        .unwrap()
        .escalation
        .iter()
        .filter(|e| !e.is_empty())
        .count();
    let forwarded = seen.lock().unwrap().forwarded.clone();
    Run {
        forwarded,
        snaps,
        unchanged_ticks,
        pixel_perfect_tiles,
        escalated,
    }
}

/// The first tick at which the two runs' states differ, with the field.
fn first_divergence(a: &Run, b: &Run) -> Option<String> {
    for (t, (x, y)) in a.snaps.iter().zip(&b.snaps).enumerate() {
        if x != y {
            let field = if x.tiles != y.tiles {
                let i = x
                    .tiles
                    .iter()
                    .zip(&y.tiles)
                    .position(|(p, q)| p != q)
                    .unwrap();
                format!(
                    "metrics of tile {i}: full {} vs damage {}",
                    x.tiles[i], y.tiles[i]
                )
            } else if x.generations != y.generations {
                format!("generations {:?} vs {:?}", x.generations, y.generations)
            } else if x.acked != y.acked {
                format!("acked {:?} vs {:?}", x.acked, y.acked)
            } else if x.escalation_asked != y.escalation_asked {
                format!(
                    "escalation candidates {:?} vs {:?}",
                    x.escalation_asked, y.escalation_asked
                )
            } else if x.forwarded != y.forwarded {
                format!(
                    "escalation forward read {:?} vs {:?}",
                    x.forwarded, y.forwarded
                )
            } else {
                "scheduler queues or frame mode".to_string()
            };
            return Some(format!("tick {t}: {field}"));
        }
    }
    None
}

#[tokio::test]
async fn unchanged_ticks_leave_the_state_machine_where_a_full_compare_does() {
    let full = run(false).await;
    let damage = run(true).await;
    // The measurement is not vacuous: the damage run really skipped pixels,
    // tiles reached PixelPerfect and idle escalation fired, in both.
    assert!(
        damage.unchanged_ticks >= TICKS - 10,
        "only {} unchanged ticks",
        damage.unchanged_ticks
    );
    assert_eq!(full.unchanged_ticks, 0);
    assert!(
        full.pixel_perfect_tiles > 0,
        "no tile reached PixelPerfect in the reference run"
    );
    assert!(
        full.escalated > 0,
        "idle escalation never fired in the reference run"
    );
    assert!(
        full.snaps.iter().any(|s| !s.forwarded.is_empty()),
        "no escalation forward ran in the reference run"
    );
    for r in [&full, &damage] {
        assert_eq!(forwards_read_the_screen(r, &r.forwarded), Ok(()));
    }
    assert!(
        full.forwarded.iter().any(|f| f.1 == 11),
        "the drifting tile (SAD reference differs from the screen) was never escalated"
    );
    assert_eq!(first_divergence(&full, &damage), None);
    assert_eq!(full.snaps, damage.snaps);
}
