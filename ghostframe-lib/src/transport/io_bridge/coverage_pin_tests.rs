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
        b.transmission_ledger.record_departed(
            e.wire_seq,
            now,
            crate::transport::transmission_ledger::Transmission {
                emit_us: e.emit_us,
                wire_bytes: e.wire_bytes,
                content: e.content,
                depart_us: None,
                dep_idx: None,
            },
        );
        out.push((e.wire_seq, e.content.tile().expect("a tile pass")));
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
    // Every emission, thin or bundled, took one reference.
    b.palette_table.ref_count[PID as usize] = (other_bundled + 1) as _;
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
        SchedulerEmissionPolicy::TestCdf53All,
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

/// A tile pass is one datagram. A payload that does not fit the path's datagram
/// size is not split: it is not sent at all (frame dispatch refuses such a path
/// first; this is the backstop), and one that fits goes out as exactly one.
#[tokio::test(start_paused = true)]
async fn a_tile_payload_larger_than_the_datagram_room_is_not_sent_and_one_that_fits_is_one_datagram(
) {
    let mut b = bridge();
    let gen = b.scheduler.bump_generation(TX, TY);
    b.scheduler.enqueue_refinement_subset_at(
        TX,
        TY,
        gen,
        0b1,
        vec![vec![0xCD; 200]],
        super::now_std(),
    );
    assert!(
        drain(&mut b, 10, 100).is_empty(),
        "200 bytes into 100: not sent"
    );
    assert!(!b.reliable_emitter.has_cache_entries_for_tile(TX, TY));

    let gen = b.scheduler.bump_generation(TX, TY);
    b.scheduler.enqueue_refinement_subset_at(
        TX,
        TY,
        gen,
        0b1,
        vec![vec![0xCD; 200]],
        super::now_std(),
    );
    assert_eq!(
        drain(&mut b, 11, 200).len(),
        1,
        "200 bytes into 200: one datagram"
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

/// A thin PalRle emission took a reference but no carry: delivery is
/// recorded, its reference is given back and the carry count is left alone.
#[tokio::test(start_paused = true)]
async fn acking_a_thin_palrle_tile_records_delivery_and_releases_only_its_reference() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, false, 1);
    assert_eq!((carry(&b), refs(&b)), (1, 2), "precondition");

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

/// A thin emission superseded and then acknowledged: delivery only. The
/// supersession gave its reference back, so the ack must not.
#[tokio::test(start_paused = true)]
async fn a_superseded_thin_palrle_ack_still_proves_delivery() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, false, 1);
    assert_eq!((carry(&b), refs(&b)), (1, 2), "precondition");
    dirty_the_tile(&mut b, 11);
    assert_eq!(
        (carry(&b), refs(&b)),
        (1, 1),
        "precondition: supersession gave the thin reference back"
    );
    ack(&mut b, &[sent[0].0]);
    assert_eq!((carry(&b), refs(&b)), (1, 1), "no second release");
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

/// Re-committing a Cdf53 tile while a bundled PalRle emission of the same
/// tile is in flight supersedes it: the carry and reference come back at the
/// commit, with no acknowledgement needed, and a late ack gives nothing back
/// a second time.
#[tokio::test(start_paused = true)]
async fn a_cdf53_commit_settles_a_bundled_palrle_emission_once() {
    let mut b = bridge();
    let sent = emit_palrle(&mut b, 10, true, 1);
    assert_eq!((carry(&b), refs(&b)), (2, 2), "precondition");
    b.enqueue_cdf53_generation(
        TX,
        TY,
        0b1,
        vec![(0, vec![7; 50])],
        frame::Cdf53Source::Dirty,
    );
    assert_eq!(
        (carry(&b), refs(&b)),
        (1, 1),
        "the commit gave this emission's carry and reference back"
    );
    assert!(b.reliable_emitter.cache.get(&sent[0].1).is_none());

    ack(&mut b, &[sent[0].0]);

    assert_eq!((carry(&b), refs(&b)), (1, 1), "no second release");
    assert!(b.palette_table.delivered.contains(PID));
}

/// A PalRle emission is lost and the tile turns photographic: the Cdf53
/// commit supersedes it and no acknowledgement ever comes, thin or bundled.
/// Every reference it took must still come back, and a later supersession of
/// the same tile must not find anything left to give back.
#[tokio::test(start_paused = true)]
async fn a_cdf53_commit_releases_an_unacked_palrle_emission_thin_or_bundled() {
    for bundled in [false, true] {
        let mut b = bridge();
        let _ = emit_palrle(&mut b, 10, bundled, 1);
        b.enqueue_cdf53_generation(
            TX,
            TY,
            0b1,
            vec![(0, vec![7; 50])],
            frame::Cdf53Source::Dirty,
        );
        assert_eq!(
            (carry(&b), refs(&b)),
            (1, 1),
            "bundled={bundled}: back to the other emission's share"
        );
        dirty_the_tile(&mut b, 11);
        assert_eq!(
            (carry(&b), refs(&b)),
            (1, 1),
            "bundled={bundled}: once only"
        );
    }
}

/// A thin emission that is never acknowledged and never superseded: the next
/// session's reset zeroes the table. An emission the new session makes under
/// the same key must then be released exactly once, whichever ack arrives.
#[tokio::test(start_paused = true)]
async fn a_session_reset_forgets_an_unacked_thin_emission_without_a_double_release() {
    let mut b = bridge();
    let old = emit_palrle(&mut b, 10, false, 0);
    assert_eq!(refs(&b), 1, "precondition");
    b.palette_table.on_session_reset(false);
    assert_eq!(refs(&b), 0, "the reset zeroed the reference");

    // The new session emits under the same key, next to one other holder.
    let new = emit_palrle(&mut b, 10, false, 1);
    assert_eq!(refs(&b), 2, "precondition");
    ack(&mut b, &[old[0].0, new[0].0]);

    assert_eq!(
        refs(&b),
        1,
        "the new emission gave its reference back once, the old one nothing"
    );
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

// ---- a long session of palette emissions ----

/// Take a palette the way the encoder does (`acquire_or_allocate`, a carry
/// only when the receiver does not have it yet), record the emission's
/// coverage as the emit path does, and acknowledge it.
fn palrle_round_trip(b: &mut IoBridge, palette: &crate::encoder::pal_rle::PaletteEntry, n: u32) {
    use crate::transport::fragment_coverage::FragmentCoverage;
    let id = b
        .palette_table
        .acquire_or_allocate(palette)
        .expect("a table whose references are all given back always has a slot");
    let bundled = !b.palette_table.delivered.contains(id);
    if bundled {
        b.palette_table.in_flight_carrying[id as usize] += 1;
    }
    let key = (1000 + n, TX, TY, 0);
    b.fragment_coverage.record(
        key,
        smallvec::smallvec![FragmentCoverage {
            tile_x: TX,
            tile_y: TY,
            generation: 0,
            pass_idx: 0,
            codec: Codec::PalRle,
            palette_id: Some(id),
            palette_bundled: bundled,
            palette_held: true,
        }],
    );
    let wire_seq = 50_000 + n;
    b.seed_transmission_for_test(wire_seq, EmitKey::new(key.0, TX, TY, 0));
    ack(b, &[wire_seq]);
}

fn palette_n(n: u32) -> crate::encoder::pal_rle::PaletteEntry {
    let mut p = crate::encoder::pal_rle::PaletteEntry::default();
    p.colors[0] = n.to_le_bytes();
    p.count = 1;
    p
}

/// Every reference an emission takes is given back by its acknowledgement,
/// thin or bundled. Emitting more distinct palettes than the table has slots
/// is only possible if each ack frees its slot for eviction.
#[tokio::test(start_paused = true)]
async fn a_long_session_of_acked_palette_emissions_returns_the_table_to_baseline() {
    let mut b = bridge();
    // Distinct palettes: three times the 256 slots. Each emission is bundled
    // the first time (the receiver has never seen it).
    for n in 0..768u32 {
        palrle_round_trip(&mut b, &palette_n(n), n);
        assert_eq!(
            b.palette_table.ref_count.iter().sum::<u32>(),
            0,
            "palette {n}: its ack left a reference behind"
        );
        assert_eq!(b.palette_table.in_flight_carrying.iter().sum::<u32>(), 0);
    }
    // The same palette, now delivered, re-emitted thin many times.
    for n in 0..2000u32 {
        palrle_round_trip(&mut b, &palette_n(767), 10_000 + n);
    }
    assert_eq!(b.palette_table.ref_count.iter().sum::<u32>(), 0);
    assert_eq!(b.palette_table.in_flight_carrying.iter().sum::<u32>(), 0);
    assert!(
        b.palette_table
            .slot_state
            .iter()
            .all(|s| *s != crate::encoder::pal_rle::SlotState::Held),
        "no slot may stay Held once everything emitted was acknowledged"
    );
}
