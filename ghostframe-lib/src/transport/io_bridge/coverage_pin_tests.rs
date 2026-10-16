//! Pins what the ACK path does with an emitted tile-pass's metadata
//! (codec, generation, palette id, bundled flag), end to end.
//!
//! Every test here emits through the real `drain_scheduler_into_quinn` (or
//! `dispatch_dirty_tiles_via_scheduler`), turns the emitter's transmissions
//! into ledger records the way production does, and acknowledges by
//! `wire_seq`. None of them reaches into the structure that stores the
//! metadata, so they hold across a change of where it is stored. The older
//! tests in `tests.rs` seed that structure directly and cannot.
//!
//! Each name states a behaviour the retirement of `FragmentCoverageMap` must
//! not change: what an acknowledgement does depends on how the emission
//! *ended* (acknowledged, superseded, retired, session lost), not only on
//! what it was.

use super::*;
use crate::transport::ack::{AckBatch, AckEntry};
use crate::transport::protocol::Codec;
use crate::transport::reliable_emitter::EmitKey;
use crate::transport::scheduler::{TileWork, WorkState};
use tokio::net::UnixStream;

const TX: u8 = 2;
const TY: u8 = 3;
const PID: u8 = 9;

fn bridge() -> IoBridge {
    let (ours, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut b = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);
    b.metrics_tracker.resize(4, 4);
    b
}

fn work(codec: Codec, generation: u8, pass_idx: u8, payload: Vec<u8>) -> TileWork {
    TileWork {
        tile_x: TX,
        tile_y: TY,
        generation,
        pass_idx,
        total_passes: 1,
        codec,
        payload,
        queued_at: super::now_std(),
        last_sent_at: None,
        state: WorkState::Pending,
    }
}

/// Production's post-turn step: everything the emitter put on the wire
/// becomes an outstanding ledger record.
fn record_transmissions(b: &mut IoBridge) -> Vec<(u32, EmitKey)> {
    let now = super::now_std();
    let mut out = Vec::new();
    for e in b.reliable_emitter.take_transmissions() {
        b.transmission_ledger.record(
            e.wire_seq,
            now,
            crate::transport::transmission_ledger::Transmission {
                emit_us: e.emit_us,
                wire_bytes: e.wire_bytes,
                key: e.key,
            },
        );
        out.push((e.wire_seq, e.key));
    }
    out
}

/// Drain whatever the scheduler holds as frame `seq`; return the
/// transmissions it produced.
fn drain(b: &mut IoBridge, seq: u32, max_frag: usize) -> Vec<(u32, EmitKey)> {
    b.drain_scheduler_into_quinn(seq, 0, max_frag, usize::MAX);
    record_transmissions(b)
}

fn ack(b: &mut IoBridge, wire_seqs: &[u32]) {
    let batch = AckBatch {
        entries: wire_seqs
            .iter()
            .map(|&wire_seq| AckEntry {
                wire_seq,
                arrival_time_ms_lo16: 0,
            })
            .collect(),
    };
    b.dispatch_ack_datagram(&batch.encode());
}

/// Emit one Cdf53 pass for the tile at the tile's next generation. Returns
/// that generation and the pass's transmissions.
fn emit_cdf53(b: &mut IoBridge, seq: u32, payload_len: usize) -> (u8, Vec<(u32, EmitKey)>) {
    let gen = b.scheduler.bump_generation(TX, TY);
    b.scheduler.enqueue_refinement_subset_at(
        TX,
        TY,
        gen,
        0b1,
        vec![vec![0xCD; payload_len]],
        super::now_std(),
    );
    (gen, drain(b, seq, 1200))
}

/// Emit one PalRle tile. `bundled` is payload byte 0 bit 0, `PID` byte 1.
/// Seeds the palette bookkeeping the way Phase A and the bundled encode path
/// leave it for the emissions it stands for: one outstanding bundled
/// emission elsewhere (`other_bundled`) plus this one if it is bundled.
fn emit_palrle(
    b: &mut IoBridge,
    seq: u32,
    bundled: bool,
    other_bundled: u32,
) -> Vec<(u32, EmitKey)> {
    let carrying = other_bundled + u32::from(bundled);
    b.palette_table.ref_count[PID as usize] = carrying as _;
    b.palette_table.in_flight_carrying[PID as usize] = carrying as _;
    b.palette_table.slot_state[PID as usize] = crate::encoder::pal_rle::SlotState::Held;
    let gen = b.scheduler.generation_for(TX, TY);
    b.scheduler.enqueue_at(
        work(
            Codec::PalRle,
            gen,
            0,
            vec![u8::from(bundled), PID, 1, 2, 3, 4],
        ),
        super::now_std(),
    );
    drain(b, seq, 1200)
}

/// Re-encode the tile through the production dirty path, which cancels the
/// emitter's state for it, bumps the generation and settles the superseded
/// palettes, then emits the replacement (Raw under the CPU policy).
fn dirty_the_tile(b: &mut IoBridge, seq: u32) {
    let pixels = vec![0x55u8; 128 * 128 * 4];
    let grid = crate::tile::TileGrid::new(128, 128);
    let _ = b.dispatch_dirty_tiles_via_scheduler(
        &[(TX as u32, TY as u32)],
        &grid,
        TileDispatchFrame {
            pixels: &pixels,
            stride: 128 * 4,
            seq,
            timestamp_us: 0,
        },
        1200,
        SchedulerEmissionPolicy::CpuRawOnly,
        None,
    );
    record_transmissions(b);
}

fn carry(b: &IoBridge) -> u32 {
    b.palette_table.in_flight_carrying[PID as usize]
}

fn refs(b: &IoBridge) -> u32 {
    b.palette_table.ref_count[PID as usize]
}

// ---- a live acknowledgement ----

/// A Cdf53 pass spans several datagrams; each is acknowledged under its own
/// `wire_seq`. The pass is delivered once, however many of its datagrams
/// come back, and the first one is the one that counts.
#[tokio::test(start_paused = true)]
async fn a_multi_datagram_cdf53_pass_is_acknowledged_once() {
    let mut b = bridge();
    let (gen, sent) = emit_cdf53(&mut b, 10, 3000);
    assert!(sent.len() >= 3, "precondition: the pass spans datagrams");
    let all: Vec<u32> = sent.iter().map(|(w, _)| *w).collect();
    let window_before = b.scheduler.delivery_window_acked_for_test();

    ack(&mut b, &all);

    assert_eq!(b.scheduler.cdf53_passes_acked_for_test(TX, TY, gen), 1);
    assert_eq!(
        b.scheduler.delivery_window_acked_for_test(),
        window_before + 1,
        "one pass, one delivery-window credit"
    );
    assert_eq!(b.reliable_emitter.stats.ack_hit, 1);
    assert_eq!(
        b.reliable_emitter.stats.ack_miss as usize,
        all.len() - 1,
        "the other datagrams find nothing left to release"
    );
}

/// The same acknowledgement arriving twice credits the pass once.
#[tokio::test(start_paused = true)]
async fn a_repeated_cdf53_ack_credits_the_pass_once() {
    let mut b = bridge();
    let (gen, sent) = emit_cdf53(&mut b, 10, 200);
    let ws = sent[0].0;
    let before = b.scheduler.delivery_window_acked_for_test();
    ack(&mut b, &[ws]);
    ack(&mut b, &[ws]);
    assert_eq!(b.scheduler.cdf53_passes_acked_for_test(TX, TY, gen), 1);
    assert_eq!(b.scheduler.delivery_window_acked_for_test(), before + 1);
}

/// A non-Cdf53 emission is marked acknowledged in the scheduler, which is
/// what stops it being re-sent every retry interval.
#[tokio::test(start_paused = true)]
async fn acking_a_solid_tile_marks_its_scheduler_work_acked() {
    let mut b = bridge();
    let gen = b.scheduler.generation_for(TX, TY);
    b.scheduler.enqueue_at(
        work(Codec::Solid, gen, 0, vec![1, 2, 3, 255]),
        super::now_std(),
    );
    let sent = drain(&mut b, 10, 1200);
    assert_eq!(sent.len(), 1);
    let state = |b: &IoBridge| {
        b.scheduler
            .peek_for_test()
            .iter()
            .find(|w| w.tile_x == TX && w.tile_y == TY)
            .map(|w| w.state)
    };
    assert_eq!(state(&b), Some(WorkState::InFlight), "precondition");

    ack(&mut b, &[sent[0].0]);

    assert_eq!(state(&b), Some(WorkState::Acked));
}

/// A bundled PalRle emission took one carry and one reference when it was
/// encoded; its acknowledgement gives both back and records delivery.
#[tokio::test(start_paused = true)]
async fn acking_a_bundled_palrle_tile_releases_its_carry_and_reference() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, true, 1);
    assert_eq!((carry(&b), refs(&b)), (2, 2), "precondition");
    assert!(!b.palette_table.delivered.contains(PID));

    ack(&mut b, &[sent[0].0]);

    assert_eq!(carry(&b), 1, "this emission's carry, not the other's");
    assert_eq!(refs(&b), 1);
    assert!(b.palette_table.delivered.contains(PID));
}

/// A thin PalRle emission took neither: delivery is recorded, the counts are
/// left alone.
#[tokio::test(start_paused = true)]
async fn acking_a_thin_palrle_tile_records_delivery_and_leaves_the_counts() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, false, 1);
    assert_eq!((carry(&b), refs(&b)), (1, 1), "precondition");

    ack(&mut b, &[sent[0].0]);

    assert_eq!((carry(&b), refs(&b)), (1, 1));
    assert!(b.palette_table.delivered.contains(PID));
}

/// An acknowledgement that names nothing the server emitted changes nothing.
#[tokio::test(start_paused = true)]
async fn an_ack_for_an_unknown_transmission_changes_nothing() {
    let mut b = bridge();
    let _ = emit_palrle(&mut b, 10, true, 0);
    ack(&mut b, &[0xDEAD_BEEF]);
    assert_eq!((carry(&b), refs(&b)), (1, 1));
    assert!(!b.palette_table.delivered.contains(PID));
}

// ---- an emission that ended some other way, then is acknowledged ----

/// The tile is re-encoded while a bundled emission is in flight: supersession
/// gives the carry and reference back once. The old datagram then arrives
/// anyway. It proves delivery but must not give them back a second time.
#[tokio::test(start_paused = true)]
async fn a_superseded_bundled_palrle_ack_proves_delivery_without_a_second_release() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, true, 1);
    assert_eq!((carry(&b), refs(&b)), (2, 2), "precondition");

    dirty_the_tile(&mut b, 11);
    assert_eq!(
        (carry(&b), refs(&b)),
        (1, 1),
        "precondition: supersession settled it once"
    );
    assert!(
        b.reliable_emitter.cache.get(&sent[0].1).is_none(),
        "precondition: the emitter let go of the superseded emission"
    );

    ack(&mut b, &[sent[0].0]);

    assert_eq!(carry(&b), 1, "the other bundled emission still carries");
    assert_eq!(refs(&b), 1);
    assert!(
        b.palette_table.delivered.contains(PID),
        "the late ack still proves the receiver has the palette"
    );
}

/// A thin emission superseded and then acknowledged: delivery only.
#[tokio::test(start_paused = true)]
async fn a_superseded_thin_palrle_ack_still_proves_delivery() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, false, 1);
    dirty_the_tile(&mut b, 11);
    let (c, r) = (carry(&b), refs(&b));
    ack(&mut b, &[sent[0].0]);
    assert_eq!((carry(&b), refs(&b)), (c, r));
    assert!(b.palette_table.delivered.contains(PID));
}

/// The emitter gives up on an emission that stayed unacknowledged too long
/// (`retire_stuck`). The metadata is not what ages out: if the datagram does
/// arrive later, it is still released exactly as it would have been.
#[tokio::test(start_paused = true)]
async fn a_retired_bundled_palrle_ack_still_releases_its_carry() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, true, 1);
    let now = super::now_std();
    let retired = b.reliable_emitter.retire_stuck(
        now + std::time::Duration::from_secs(600),
        std::time::Duration::from_secs(60),
    );
    assert_eq!(retired, vec![sent[0].1], "precondition");

    ack(&mut b, &[sent[0].0]);

    assert_eq!(carry(&b), 1);
    assert_eq!(refs(&b), 1);
    assert!(b.palette_table.delivered.contains(PID));
}

/// The two ways a Cdf53 tile's emissions can be superseded differ in what a
/// late acknowledgement is worth, and the estimator can see it: the delivery
/// window credits the pass only on one of them.
///
/// Escalation / dirty commit to Cdf53 (`commit_tile_to_cdf53`): the bytes did
/// reach the client, a later supersession does not un-deliver them, so the
/// late ack still counts toward the window.
#[tokio::test(start_paused = true)]
async fn a_late_cdf53_ack_after_a_cdf53_commit_still_credits_the_window() {
    let mut b = bridge();
    let (gen, sent) = emit_cdf53(&mut b, 10, 200);
    let before = b.scheduler.delivery_window_acked_for_test();

    b.enqueue_cdf53_generation(
        TX,
        TY,
        0b1,
        vec![(0, vec![7; 50])],
        frame::Cdf53Source::Dirty,
    );
    assert_ne!(
        b.scheduler.generation_for(TX, TY),
        gen,
        "precondition: the generation moved"
    );
    assert!(
        b.reliable_emitter.cache.get(&sent[0].1).is_none(),
        "precondition: the emitter let go of the emission"
    );

    ack(&mut b, &[sent[0].0]);

    assert_eq!(b.scheduler.delivery_window_acked_for_test(), before + 1);
    assert_eq!(
        b.scheduler.cdf53_passes_acked_for_test(TX, TY, gen),
        0,
        "the old generation's completeness mask is not the live one's"
    );
}

/// Re-encoding the tile through the dirty path discards the tile's
/// not-yet-acknowledged Cdf53 emissions: their late acknowledgement is
/// not credited.
#[tokio::test(start_paused = true)]
async fn a_late_cdf53_ack_after_a_dirty_re_encode_is_not_credited() {
    let mut b = bridge();
    let (_gen, sent) = emit_cdf53(&mut b, 10, 200);
    dirty_the_tile(&mut b, 11);
    let before = b.scheduler.delivery_window_acked_for_test();

    ack(&mut b, &[sent[0].0]);

    assert_eq!(b.scheduler.delivery_window_acked_for_test(), before);
}

/// The dirty path drops only Cdf53 emissions' metadata: a PalRle emission of
/// the same tile survives it (covered above), and so does a Cdf53 emission of
/// another tile.
#[tokio::test(start_paused = true)]
async fn dirtying_one_tile_leaves_another_tiles_cdf53_ack_intact() {
    let mut b = bridge();
    let other_gen = b.scheduler.bump_generation(0, 0);
    b.scheduler.enqueue_refinement_subset_at(
        0,
        0,
        other_gen,
        0b1,
        vec![vec![1; 100]],
        super::now_std(),
    );
    let sent = drain(&mut b, 10, 1200);
    assert_eq!(sent.len(), 1);

    dirty_the_tile(&mut b, 11);
    ack(&mut b, &[sent[0].0]);

    assert_eq!(b.scheduler.cdf53_passes_acked_for_test(0, 0, other_gen), 1);
}

/// Today the session's end empties the emitter's cache but nothing else, so
/// an acknowledgement that still resolves through the ledger afterwards
/// completes its bookkeeping. Recorded so a change here is a decision.
#[tokio::test(start_paused = true)]
async fn an_ack_after_the_cache_was_cleared_still_completes_its_bookkeeping() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, true, 1);
    b.reliable_emitter.clear_cache();

    ack(&mut b, &[sent[0].0]);

    assert_eq!(carry(&b), 1);
    assert_eq!(refs(&b), 1);
    assert!(b.palette_table.delivered.contains(PID));
}

/// Re-committing a Cdf53 tile (which cancels the emitter and bumps the
/// generation but settles no palettes) while a bundled PalRle emission of the
/// same tile is in flight leaves that emission's acknowledgement as the only
/// thing that gives its carry and reference back.
#[tokio::test(start_paused = true)]
async fn a_cdf53_commit_leaves_a_bundled_palrle_ack_to_release_its_carry() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, true, 1);
    b.enqueue_cdf53_generation(
        TX,
        TY,
        0b1,
        vec![(0, vec![7; 50])],
        frame::Cdf53Source::Dirty,
    );
    assert_eq!(
        (carry(&b), refs(&b)),
        (2, 2),
        "precondition: nothing settled it"
    );
    assert!(b.reliable_emitter.cache.get(&sent[0].1).is_none());

    ack(&mut b, &[sent[0].0]);

    assert_eq!((carry(&b), refs(&b)), (1, 1));
    assert!(b.palette_table.delivered.contains(PID));
}

/// A retired Solid emission that arrives after all is still marked acked, so
/// the scheduler stops re-sending it.
#[tokio::test(start_paused = true)]
async fn a_retired_solid_ack_still_marks_its_scheduler_work_acked() {
    let mut b = bridge();
    let gen = b.scheduler.generation_for(TX, TY);
    b.scheduler.enqueue_at(
        work(Codec::Solid, gen, 0, vec![1, 2, 3, 255]),
        super::now_std(),
    );
    let sent = drain(&mut b, 10, 1200);
    let retired = b.reliable_emitter.retire_stuck(
        super::now_std() + std::time::Duration::from_secs(600),
        std::time::Duration::from_secs(60),
    );
    assert_eq!(retired.len(), 1, "precondition");

    ack(&mut b, &[sent[0].0]);

    let st = b
        .scheduler
        .peek_for_test()
        .iter()
        .find(|w| w.tile_x == TX && w.tile_y == TY)
        .map(|w| w.state);
    assert_eq!(st, Some(WorkState::Acked));
}

/// A retired Cdf53 pass that arrives after all still counts: live-generation
/// completeness and the delivery window.
#[tokio::test(start_paused = true)]
async fn a_retired_cdf53_ack_still_credits_the_pass() {
    let mut b = bridge();
    let (gen, sent) = emit_cdf53(&mut b, 10, 200);
    let before = b.scheduler.delivery_window_acked_for_test();
    let retired = b.reliable_emitter.retire_stuck(
        super::now_std() + std::time::Duration::from_secs(600),
        std::time::Duration::from_secs(60),
    );
    assert_eq!(retired.len(), 1, "precondition");

    ack(&mut b, &[sent[0].0]);

    assert_eq!(b.scheduler.cdf53_passes_acked_for_test(TX, TY, gen), 1);
    assert_eq!(b.scheduler.delivery_window_acked_for_test(), before + 1);
}
