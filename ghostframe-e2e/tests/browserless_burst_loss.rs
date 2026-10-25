//! Does the retransmit machinery treat burst loss differently from i.i.d.
//! loss at the same long-run rate?
//!
//! Three runs of the same sustained tile load: a clean control, ~2% s2c i.i.d.
//! loss, and ~2% s2c Gilbert-Elliott loss with mean burst 8 datagrams. Loss is
//! on the server -> client direction only.
//!
//! These scenes REPORT (println!, use `--nocapture`); they do not assert
//! timing or recovery thresholds. They assert only premises that make the
//! report meaningful: the session came up, the lossy runs dropped something,
//! the burst run's measured mean loss run is clearly longer than the iid
//! run's, and the control dropped nothing.
//!
//! Harness caveats: only `NetSim::decide` is seed-reproducible, not the scene
//! (see `netsim/mod.rs`), so counters vary run to run; the virtual clock is
//! paused (a healthy scene is ~350 loop iterations per virtual second).

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::FrameScript;
use ghostframe_e2e::harness::browserless::{
    run_browserless_with_links, BrowserlessResult, BrowserlessScene, LinkOverrides, SceneLoad,
};
use ghostframe_e2e::harness::load_profile::{
    gradient_tile, Churn, LoadProfile, PRODUCTION_CADENCE_US,
};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{DropPlan, DropRule, GilbertElliott, NetProfile};

const COLS: u32 = 16;
const ROWS: u32 = 16;
const RATE: f64 = 0.02;
const BURST: f64 = 8.0;

fn scene(seed: u64, secs: u64) -> BrowserlessScene {
    BrowserlessScene {
        seed,
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Profile(LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::Region { tiles_per_tick: 32 },
        }),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            delay_us: 10_000,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(secs),
        grid_cols: COLS,
        grid_rows: ROWS,
    }
}

/// Loss on the s2c direction only; c2s keeps the scene's clean profile.
fn s2c_only(mutate: impl FnOnce(&mut NetProfile)) -> LinkOverrides {
    let mut p = scene(0, 1).net;
    mutate(&mut p);
    LinkOverrides {
        s2c: Some(p),
        ..Default::default()
    }
}

/// What reached the screen, from the client's events: decoded tile updates,
/// the distinct (tile, frame) pairs among them, and the updates beyond the
/// first of a pair (refinement passes of a tile already shown). Fewer bytes
/// for the same figures is less waste; fewer figures is content lost.
fn content_delivered(r: &BrowserlessResult) -> (usize, usize, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    for e in &r.events {
        if let ClientNetEvent::Core(ghostframe_client_core::Event::TileReady {
            frame_seq,
            tile_x,
            tile_y,
            ..
        }) = e
        {
            total += 1;
            seen.insert((*frame_seq, *tile_x, *tile_y));
        }
    }
    (total, seen.len(), total - seen.len())
}

fn tiles_present(r: &BrowserlessResult) -> usize {
    (0..COLS as u8)
        .flat_map(|x| (0..ROWS as u8).map(move |y| (x, y)))
        .filter(|&(x, y)| r.framebuffer.tile_rgba(x, y).is_some())
        .count()
}

fn report(name: &str, r: &BrowserlessResult) {
    let ratio = if r.s2c_datagrams_dropped == 0 {
        0.0
    } else {
        r.retransmit_attempts_total as f64 / r.s2c_datagrams_dropped as f64
    };
    println!(
        "BURSTREPORT {name}: offered={} dropped={} realised_loss={:.4} loss_runs={} \
         mean_run={:.3} max_run={} retransmit_attempts_total={} nack_hit={} nack_miss={} \
         rto_fired={} retx_per_drop={:.3} tiles_present={}/{} stale={} \
         s2c_bytes={} s2c_dg={} total_bytes={} retx_nack={} rto_spurious={} rto_useful={} \
         quinn_cwnd={} quinn_cong={} quinn_lost_pk={} queue_max_B={} client_lost_pre_fec={} \
         client_run_hist={:?} client_recovered={} client_dup_rebuilt={} client_rx={:?}",
        r.s2c_datagrams_offered,
        r.s2c_datagrams_dropped,
        r.s2c_datagrams_dropped as f64 / r.s2c_datagrams_offered.max(1) as f64,
        r.s2c_loss_runs,
        r.s2c_mean_loss_run(),
        r.s2c_loss_run_max,
        r.retransmit_attempts_total,
        r.nack_hit,
        r.nack_miss,
        r.rto_fired,
        ratio,
        tiles_present(r),
        COLS * ROWS,
        r.stale_generation_tiles,
        r.bytes_delivered_s2c,
        r.datagrams_delivered_s2c,
        r.bytes_delivered,
        r.retransmit_by_nack,
        r.rto_spurious,
        r.rto_useful,
        r.transport.cwnd,
        r.transport.congestion_events,
        r.transport.lost_packets,
        r.transport.max_queue_bytes,
        r.client_wire_loss.lost_pre_fec,
        r.client_wire_loss.run_hist,
        r.client_wire_loss.recovered,
        r.client_wire_loss.dup_rebuilt,
        r.client_rx_datagrams,
    );
}

/// The D1 instruments, one line per run (see `ghostframe_lib::transport::instruments`).
fn report_instruments(name: &str, r: &BrowserlessResult) {
    let i = &r.transport.instr;
    use ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass as W;
    println!(
        "D1INSTR {name}: window_ms={} fifo_samples={} fifo_max_B={} fifo_peak_at_enqueue_B={} \
         fifo_mean_B={} fifo_slope_last_quarter_Bps={} quinn_limited[turns={:.3} time={:.3}] \
         offered_Bps[crit={:.0} refine={:.0} nack={:.0} rto={:.0} sched_retry={:.0} parity={:.0} h264={:.0} \
         control={:.0} overhead={:.0}] offered_total_B={} quinn_enqueued_B={} \
         quinn_departed_Bps={:.0} lag_us[n={} p50={} p95={} max={}] mirror_len={} \
         mirror_overflowed={} budget_passes[cwnd={} pacer={} floor={} probe={}] gate_stops[depth={} tokens={} rejected={}] \
         admission[pumps={} admitted={} d_target_max_B={} depth_violations={} backlog_turns={} starved_turns={} \
         charged_B={} soft_charged_B={} ungated_at_depth={}] \
         ledger[declared={} flushed={} overflowed={} late_before_flush={} late_after_flush={} \
         late_unreported={} unknown_acks={} tombstone_evictions={} expired_before_departure={} \
         departures_stamped={} evicted_before_departure={}] \
         loss[pkt={} time={} pto={} k_pkt={} k_raises={} spurious_ledger={} loss_retx={} probe_retx={} pto_fires={} \
         verdict_no_retx[gone={} newer={} queued={}] spurious_loss={} spurious_pto={}] \
         ecn[ect0={} unmarked={} last_marked={}] rto_fired={} rto_spurious={} rto_useful={} \
         generation_replacements={} rto_fired_superseded={}",
        i.window_us / 1000,
        i.fifo_samples,
        i.fifo_depth_max,
        i.fifo_depth_peak_at_enqueue,
        i.fifo_depth_mean,
        i.fifo_depth_slope_last_quarter_bps,
        i.quinn_limited_turn_fraction(),
        i.quinn_limited_time_fraction(),
        i.per_second(i.offered.of(W::Critical)),
        i.per_second(i.offered.of(W::Refinement)),
        i.per_second(i.offered.of(W::NackRepair)),
        i.per_second(i.offered.of(W::RtoRetransmit)),
        i.per_second(i.offered.of(W::SchedulerRetry)),
        i.per_second(i.offered.of(W::Parity)),
        i.per_second(i.offered.of(W::H264)),
        i.per_second(i.offered.of(W::Control)),
        i.per_second(i.offered.overhead),
        i.offered.total(),
        i.quinn_bytes_enqueued,
        i.per_second(i.quinn_bytes_departed),
        i.lag_samples,
        i.lag_p50_us,
        i.lag_p95_us,
        i.lag_max_us,
        i.mirror_len,
        i.mirror_overflowed,
        i.budget.cwnd,
        i.budget.pacer,
        i.budget.floor,
        i.budget.probe,
        i.budget.gate_depth,
        i.budget.gate_tokens,
        i.budget.gate_rejected,
        i.admission.pumps,
        i.admission.datagrams_admitted,
        i.admission.d_target_max,
        i.admission.depth_violations,
        i.admission.backlog_turns,
        i.admission.starved_turns,
        i.admission.charged_bytes,
        i.admission.soft_charged_bytes,
        i.admission.ungated_at_depth,
        i.ledger_losses_declared,
        i.ledger_losses_flushed,
        i.ledger_losses_overflowed,
        i.late_acked_before_flush,
        i.late_acked_after_flush,
        i.late_acked_unreported,
        i.ledger_unknown_acks,
        i.ledger_tombstone_evictions,
        i.ledger_expired_before_departure,
        i.ledger_departures_stamped,
        i.ledger_evicted_before_departure,
        i.loss_by_packet_threshold,
        i.loss_by_time_threshold,
        i.loss_by_pto,
        i.k_pkt,
        i.k_pkt_raises,
        i.ledger_spurious_losses,
        i.loss_retransmits,
        i.pto_probe_retransmits,
        i.pto_fires,
        i.verdict_content_gone,
        i.verdict_newer_in_flight,
        i.verdict_already_queued,
        i.spurious_loss,
        i.spurious_pto,
        i.ecn.ect0_tx,
        i.ecn.unmarked_tx,
        i.ecn.last_marked,
        r.rto_fired,
        r.rto_spurious,
        r.rto_useful,
        i.generation_replacements,
        i.rto_fired_superseded,
    );
}

/// P3 gate: the ledger never declares a transmission lost while it is still
/// queued (the loss clock starts at departure).
///
/// What the counter guards: `expired_before_departure` is read from the
/// record at the moment of a verdict. It is zero by construction of the
/// departure-ordered walk in `expire_with_seq`; it does not exercise the
/// tracker (that rests on the real-quinn proptest in `departure.rs`). It
/// goes positive if the walk returns to recording order (the mutation
/// recorded in the PR: 172 at burst 2 %, failing the assertion below), and
/// only when queued transmissions are in the FIFO and verdicts are issued,
/// which is what the premises establish. The FIFO must have been non-empty,
/// departures stamped, timers started by them, and (on lossy runs) the clock
/// must really have issued verdicts, or the zero proves nothing.
fn assert_p3(n: &str, r: &BrowserlessResult, lossy: bool) {
    let i = &r.transport.instr;
    assert!(
        i.fifo_depth_max > 0 && i.quinn_limited_turns > 0,
        "{n}: P3 premise: the FIFO was never non-empty"
    );
    assert!(
        i.ledger_departures_stamped > 0,
        "{n}: P3 premise: no departure reached the ledger"
    );
    if lossy {
        assert!(
            i.ledger_losses_declared > 0,
            "{n}: P3 premise: no verdict was issued, so the zero proves nothing"
        );
    }
    assert_eq!(
        i.ledger_expired_before_departure, 0,
        "{n}: transmissions declared lost while still queued"
    );
}

/// Premise for the D1 instruments: a counter that never moves is a dead probe,
/// and reads exactly like a healthy link. Everything asserted here is exact or
/// structural, not a threshold: the scene is stepped and reproducible.
///
/// Legitimately zero in this scene, and so NOT asserted here:
/// - `late_acked_after_flush`: a late acknowledgement for a loss goog_cc had
///   already been told about. Here the pending list overflows its cap first
///   (most losses never reach goog_cc), and the few that do are real. It is
///   pinned by `a_late_ack_after_the_flush_is_counted_apart_from_one_before_it`
///   in ghostframe-lib, which drives the production turn.
/// - `budget.floor`: the floor is one datagram, far below the bandwidth term
///   on this link. Asserted in `the_budget_floor_term_is_counted_when_it_binds`.
/// - `budget.pacer`: binds in some runs only (the pacer is 1.0 x the estimate,
///   bounded by `K = 1/0.85` x the acknowledged rate);
///   asserted over the lossy runs together by the caller.
/// - ECN `last_marked`: ghostbridge carries no ECN, so quinn abandons ECN
///   after the first acknowledgements (`ect0_tx` is the handshake's few
///   packets); asserted only that both marked and unmarked transmits exist.
fn assert_instruments(n: &str, r: &BrowserlessResult, lossy: bool) {
    use ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass as W;
    let i = &r.transport.instr;
    // FIFO series.
    assert!(
        i.fifo_samples > 0 && i.window_us > 0,
        "{n}: no FIFO readings"
    );
    assert!(i.fifo_depth_max > 0, "{n}: FIFO depth never non-zero");
    assert!(
        i.fifo_depth_peak_at_enqueue >= i.fifo_depth_max,
        "{n}: the peak at enqueue cannot be below the post-drain maximum"
    );
    assert!(i.fifo_depth_mean > 0, "{n}: time-weighted mean is zero");
    assert!(
        i.fifo_depth_slope_last_quarter_bps != 0,
        "{n}: slope is zero"
    );
    assert!(i.quinn_limited_turns > 0, "{n}: no quinn-limited turn");
    assert!(i.quinn_limited_turns <= i.fifo_samples);
    assert!(
        i.quinn_limited_us > 0 && i.quinn_limited_us <= i.window_us,
        "{n}: quinn-limited time {} of {} us",
        i.quinn_limited_us,
        i.window_us
    );
    // Offered load against quinn: every byte billed, none twice.
    for c in [W::Critical, W::Refinement, W::Parity, W::Control] {
        assert!(i.offered.of(c) > 0, "{n}: {c:?} never billed");
    }
    assert!(i.offered.overhead > 0, "{n}: no framing overhead billed");
    assert_eq!(
        i.offered.total(),
        i.quinn_bytes_enqueued,
        "{n}: classes + overhead must equal the bytes quinn accepted"
    );
    assert!(
        i.quinn_bytes_departed > 0 && i.quinn_bytes_departed <= i.quinn_bytes_enqueued,
        "{n}: departed {} of {} enqueued",
        i.quinn_bytes_departed,
        i.quinn_bytes_enqueued
    );
    // Departure tracker: every datagram quinn accepted for the live session
    // (tiles, parity, H.264, dimensions, pongs) is either stamped or still
    // queued. Eviction notices are the one exception and none are sent here.
    let all: u64 = i.offered.datagrams.iter().sum();
    assert!(i.lag_samples > 0, "{n}: no departure was stamped");
    assert_eq!(
        i.lag_samples + i.mirror_len,
        all,
        "{n}: tracker lost track of datagrams: stamped {} + queued {}",
        i.lag_samples,
        i.mirror_len
    );
    assert_eq!(i.mirror_overflowed, 0, "{n}: mirror hit its cap");
    assert!(
        i.lag_p95_us >= i.lag_p50_us && i.lag_p95_us > 0 && i.lag_max_us >= i.lag_p50_us,
        "{n}: lag p50 {} p95 {} max {}",
        i.lag_p50_us,
        i.lag_p95_us,
        i.lag_max_us
    );
    assert_p3(n, r, lossy);
    // Budget binding: every tick lands in exactly one term.
    assert!(
        i.budget.ticks() > 0 && i.budget.cwnd > 0,
        "{n}: no budget ticks"
    );
    assert!(i.budget.probe > 0, "{n}: no probe-bound tick");
    // ECN.
    assert!(
        i.ecn.ect0_tx > 0 && i.ecn.unmarked_tx > 0,
        "{n}: ECN transmits not observed"
    );
    if lossy {
        for c in [W::NackRepair, W::RtoRetransmit] {
            assert!(i.offered.of(c) > 0, "{n}: {c:?} never billed under loss");
        }
        assert!(i.ledger_losses_declared > 0, "{n}: no ledger loss");
        // Not asserted any more: that the pending-loss cap overflowed and the
        // tombstone ring evicted. Both were symptoms of the standing queue
        // (76,821 losses declared against 102 real drops, 61,976 of them past
        // the cap), and admission removes the queue. They are still reported.
        assert!(
            i.ledger_unknown_acks > 0,
            "{n}: no late ack went unresolved"
        );
        assert!(
            i.budget.gate_depth > 0 || i.budget.gate_tokens > 0,
            "{n}: admission never held work back under loss"
        );
        assert!(i.ledger_losses_flushed > 0, "{n}: no loss reached goog_cc");
        assert!(
            i.ledger_losses_flushed + i.ledger_losses_overflowed <= i.ledger_losses_declared,
            "{n}: more losses accounted for than declared"
        );
        // Not asserted any more: that some late ack withdrew a pending loss
        // (`late_acked_before_flush > 0`; 354 / 160 for seed b057, iid / burst,
        // before the departure tracker). Those were transmissions declared lost
        // by a clock that started at enqueue while they stood in a queue, then
        // acknowledged once they had been sent. The clock starts at departure
        // now, so a transmission is declared lost only after a horizon on the
        // wire, and none is acknowledged after that on this 10 ms path
        // (0 / 0, and 0 in the other two seeds). The mechanism itself is pinned
        // by the lib tests `a_late_ack_retracts_the_pending_loss_it_contradicts`
        // and `a_late_ack_after_the_flush_is_counted_apart_from_one_before_it`.
        // The tail, not the median, and weaker than it was: the median queue
        // is gone (p50 was 7-16 ms, is 0-3 ms), and the log-spaced histogram's
        // p95 reads the same 20.5 ms bucket on either side of this change, so
        // it no longer tells a standing queue from none. What still does is
        // the longest wait: a datagram waited at least the gate's own depth,
        // `QUEUE_TARGET` = 10 ms, which a FIFO that never held more than a
        // datagram or two could not produce (0 on a clean link's median, tens
        // of ms to seconds here in every lossy run).
        assert!(
            i.lag_max_us >= 10_000,
            "{n}: no datagram ever waited the gate's depth in the FIFO (max lag {} us)",
            i.lag_max_us
        );
    }
}

fn assert_established(name: &str, r: &BrowserlessResult) {
    assert!(
        r.events.contains(&ClientNetEvent::SessionReady),
        "{name}: session never established"
    );
    assert!(r.s2c_datagrams_offered > 0, "{name}: no s2c traffic at all");
}

#[tokio::test(start_paused = true)]
async fn iid_versus_burst_loss_at_equal_average_rate() {
    const SECS: u64 = 12;
    let seed = 0xB0_B057;

    let clean = run_browserless_with_links(scene(seed, SECS), LinkOverrides::default())
        .await
        .expect("scene ran");
    report("clean", &clean);
    report_instruments("clean", &clean);

    let iid = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| p.loss = RATE))
        .await
        .expect("scene ran");
    report("iid2%", &iid);
    report_instruments("iid2%", &iid);

    let ge = GilbertElliott::from_rate_and_burst(RATE, BURST, 1.0).expect("feasible");
    let burst = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| ge.apply_to(p)))
        .await
        .expect("scene ran");
    report("burst2%_len8", &burst);
    report_instruments("burst2%_len8", &burst);

    for (n, r) in [("clean", &clean), ("iid", &iid), ("burst", &burst)] {
        assert_established(n, r);
    }
    // The transport counters must have been exercised, not merely printed: a
    // counter that is never incremented reads as a clean link.
    for (n, r) in [("clean", &clean), ("iid", &iid), ("burst", &burst)] {
        assert!(
            r.transport.samples > 0,
            "{n}: the transport probe never read a connection"
        );
        assert!(
            r.transport.sent_packets > 0,
            "{n}: no quinn packets counted"
        );
        assert!(
            r.client_wire_loss.received > 0,
            "{n}: client counted no datagrams"
        );
        assert_eq!(
            r.retransmit_by_nack + r.rto_fired,
            r.retransmit_attempts_total,
            "{n}: every retransmission is a NACK's or the RTO's"
        );
        let (frames_rx, app_reads) = r.client_rx_datagrams.expect("client connection");
        assert!(
            frames_rx >= app_reads && app_reads > 0,
            "{n}: rx counts {frames_rx}/{app_reads}"
        );
    }
    // Superseded content must not be retransmitted. The browserless
    // harness's stand-in for capture (`apply_injected_frame`) once
    // superseded the scheduler's queue but not the retransmit cache, so every
    // earlier generation of a tile stayed cached and RTO re-sent it for up to
    // 30 s. The client drops a stale-generation pass without acknowledging
    // it, so those retransmits were never settled, and at an age of 8-9
    // generations the client's 4-bit compare reads them as newer and
    // poisons the tile. Measured with the cancel missing: iid
    // `stale_generation_tiles` 800-1400, RTO fires ~9,000; with it, 0 and ~350.
    for (n, r) in [("clean", &clean), ("iid", &iid), ("burst", &burst)] {
        let i = &r.transport.instr;
        assert!(
            i.generation_replacements > 0,
            "{n}: premise: no tile changed generation, so the checks below prove nothing"
        );
        assert_eq!(
            i.rto_fired_superseded, 0,
            "{n}: the RTO re-sent a pass of a generation its tile had moved past"
        );
        assert_eq!(
            r.stale_generation_tiles, 0,
            "{n}: the client received passes of superseded generations"
        );
    }
    assert_instruments("clean", &clean, false);
    assert_instruments("iid", &iid, true);
    assert_instruments("burst", &burst, true);
    assert!(
        iid.transport.instr.budget.pacer + burst.transport.instr.budget.pacer > 0,
        "the pacer term never bound in either lossy run"
    );
    // Deliberately no zero-assertions on the clean run: the frame worker is a
    // real thread, so on a loaded host virtual time can jump while the loop
    // waits for it and an RTO or a quinn loss-detection event can fire with
    // nothing dropped. The lossy runs below must exceed it by construction.
    for (n, r) in [("iid", &iid), ("burst", &burst)] {
        assert!(
            r.client_wire_loss.lost_pre_fec > 0,
            "{n}: client saw no gaps under loss"
        );
        assert!(
            r.client_wire_loss.run_hist.iter().sum::<u64>() > 0,
            "{n}: no loss run recorded"
        );
        assert!(
            r.transport.congestion_events > 0,
            "{n}: loss must have cut the window"
        );
        assert!(
            r.transport.max_queue_bytes > 0,
            "{n}: queue depth never measured"
        );
        assert!(
            r.rto_fired > 0,
            "{n}: premise: the RTO fires under loss in this scene"
        );
        // Not asserted any more: that some RTO fires were spurious. They were
        // 13,760 of 21,256 (iid) before admission, fires for datagrams still
        // queued behind a 2 MB FIFO; with the FIFO bounded there are none to
        // count, and the counter is exercised by the lib tests
        // (`an_ack_for_the_original_after_an_rto_is_a_spurious_fire`).
        assert!(
            r.rto_useful > 0,
            "{n}: {} RTO fires, none useful",
            r.rto_fired
        );
        assert!(
            r.rto_spurious + r.rto_useful <= r.rto_fired,
            "{n}: more fires attributed than fired"
        );
        assert!(
            r.transport.client_received > 0,
            "{n}: no client feedback totals"
        );
    }
    assert_eq!(
        clean.s2c_datagrams_dropped, 0,
        "control link must not drop anything"
    );
    assert!(iid.s2c_datagrams_dropped > 0, "iid run dropped nothing");
    assert!(burst.s2c_datagrams_dropped > 0, "burst run dropped nothing");
    assert!(
        burst.s2c_loss_runs >= 10,
        "premise: only {} burst loss runs",
        burst.s2c_loss_runs
    );
    assert!(
        burst.s2c_mean_loss_run() > iid.s2c_mean_loss_run() * 2.0,
        "measurement must move with the model: iid mean run {:.2}, burst {:.2}",
        iid.s2c_mean_loss_run(),
        burst.s2c_mean_loss_run()
    );
}

/// With the floor pinned far above the bandwidth term, the bandwidth term can
/// never be what binds, and the floor is counted on the ticks where nothing
/// ranks above it. It is NOT the term that sets most budgets: once the pacer
/// is trusted (150 samples) the pacer's min ranks first and takes the rest of
/// the ticks (floor=2, pacer=115 in the recorded run).
/// Report-only sweep over the three committed seeds of the burst-loss
/// comparison (`0xB0B057`, `0xB0B058`, `0xB0B059`): iid 2 %, Gilbert-Elliott
/// 2 % and iid 1 % s2c loss, one line per run in the `BURSTREPORT` /
/// `D1INSTR` formats above. Used to compare a change against its base on the
/// same scenes: the burst rows are draws from a chaotic regime, so one seed
/// says little. Asserts only that each run established a session.
///
/// `#[ignore]`d (about 9 runs of 12 s): `-- --ignored --nocapture`.
#[tokio::test(start_paused = true)]
#[ignore = "report-only seed sweep, ~9 scenes; run on demand"]
async fn burst_loss_report_over_three_seeds() {
    const SECS: u64 = 12;
    let ge = GilbertElliott::from_rate_and_burst(RATE, BURST, 1.0).expect("feasible");
    for seed in [0xB0_B057u64, 0xB0_B058, 0xB0_B059] {
        let iid2 = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| p.loss = RATE))
            .await
            .expect("scene ran");
        let burst2 = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| ge.apply_to(p)))
            .await
            .expect("scene ran");
        let iid1 = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| p.loss = 0.01))
            .await
            .expect("scene ran");
        for (n, r) in [("iid2%", &iid2), ("burst2%", &burst2), ("iid1%", &iid1)] {
            let name = format!("{n}@{seed:#x}");
            report(&name, r);
            report_instruments(&name, r);
            let b = &r.transport.instr.bwe;
            println!(
                "LIFTS {name}: down={} up={} up_checked={} up_undone_next_feedback={} up_blocked_by_queue={} down_skipped_gap={}",
                b.ceiling_reseeds_down,
                b.ceiling_reseeds_up,
                b.ceiling_lifts_checked,
                b.ceiling_lifts_undone,
                b.ceiling_lifts_blocked_queue,
                b.ceiling_reseeds_skipped_gap
            );
            println!(
                "LATENCY {name}: critical n={} mean_us={} max_us={} refinement n={} mean_us={} max_us={} retx_B[nack={} rto={}] rto_original_first={}",
                r.critical_latency_count,
                r.critical_latency_mean_us,
                r.critical_latency_max_us,
                r.refinement_latency_count,
                r.refinement_latency_mean_us,
                r.refinement_latency_max_us,
                r.transport.instr.offered.of(
                    ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass::NackRepair
                ),
                r.transport.instr.offered.of(
                    ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass::RtoRetransmit
                ),
                r.rto_original_first,
            );
            let (ready, distinct, refinements) = content_delivered(r);
            println!(
                "CONTENT {name}: tile_ready={ready} distinct_tile_frames={distinct} refinement_updates={refinements} s2c_bytes={} bytes_per_distinct_tile_frame={:.0}",
                r.bytes_delivered_s2c,
                r.bytes_delivered_s2c as f64 / distinct.max(1) as f64
            );
            assert_established(&name, r);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_budget_floor_term_is_counted_when_it_binds() {
    let mut sc = scene(0xB0_B057, 4);
    sc.tick_budget_floor_bytes = Some(1 << 22);
    let r = run_browserless_with_links(sc, LinkOverrides::default())
        .await
        .expect("scene ran");
    assert_established("floor", &r);
    let b = r.transport.instr.budget;
    println!("D1INSTR floor_pinned: budget={b:?}");
    assert!(b.floor > 0, "floor never bound with the floor pinned high");
    assert_eq!(
        b.cwnd, 0,
        "the bandwidth term cannot bind below a pinned floor"
    );
}

/// A send buffer far below what the old path queued: with the depth gate the
/// FIFO never gets near it, so quinn is never asked to refuse a datagram.
/// Before admission the same scene made quinn refuse 355 and the send-buffer
/// clamp cut 176 budgets; both mechanisms are gone and `Blocked` is now a bug
/// signal. The premises are that the gate held work back (`gate_depth`) and
/// that the buffer is smaller than the FIFO would have grown without it.
///
/// `offered_scheduler_retry` and `offered_h264` are zero in every browserless
/// scene (the scheduler's fallback retry needs the emitter to have lost a
/// cache entry; the harness load is tile frames, never H.264); the retry label
/// is pinned by `a_scheduler_retry_is_labelled_apart_from_a_first_send`.
#[tokio::test(start_paused = true)]
async fn a_send_buffer_smaller_than_the_unbounded_queue_is_never_asked_to_block() {
    // 64 KiB: still 30 to 80 times under the 2-5 MB the old path queued. It
    // was 32 KiB while `D_target` stayed under it; with the
    // admission rate no longer held to 0.9 x cwnd / srtt, `D_target` (10 ms of
    // the rate) reaches 38 kB on this uncapped link.
    const BUFFER: usize = 64 * 1024;
    let mut sc = scene(0xB0_B057, 6);
    sc.datagram_send_buffer_bytes = Some(BUFFER);
    let r = run_browserless_with_links(sc, LinkOverrides::default())
        .await
        .expect("scene ran");
    assert_established("small_buffer", &r);
    let i = &r.transport.instr;
    println!(
        "D1INSTR small_buffer: budget={:?} admission={:?} send_errs={}",
        i.budget, i.admission, r.send_datagram_errs
    );
    assert!(
        i.budget.gate_depth > 0,
        "premise: the depth gate held work back"
    );
    assert!(
        (i.admission.d_target_max as usize) < BUFFER,
        "premise: D_target ({}) must fit the buffer ({BUFFER}) or the gate cannot be what spares it",
        i.admission.d_target_max
    );
    assert_eq!(r.send_datagram_errs, 0, "quinn refused a datagram");
    assert_eq!(i.budget.gate_rejected, 0);
    assert_eq!(i.admission.depth_violations, 0);
    assert_eq!(i.offered.total(), i.quinn_bytes_enqueued);
}

// ---------------------------------------------------------------------------
// F1 gates (D1 PR-2): admission bounds quinn's FIFO and never leaves it idle.
// ---------------------------------------------------------------------------

/// The figures the F1 gates are compared across branches on.
fn report_p2(name: &str, r: &BrowserlessResult) {
    let i = &r.transport.instr;
    println!(
        "P2 {name}: s2c_bytes={} s2c_dg={} total_bytes={} dropped={} fifo_max={} \
         fifo_peak_at_enqueue={} lag_p50_us={} lag_p95_us={} rto_fired={} rto_spurious={} \
         rto_useful={} quinn_cwnd={}",
        r.bytes_delivered_s2c,
        r.datagrams_delivered_s2c,
        r.bytes_delivered,
        r.s2c_datagrams_dropped,
        i.fifo_depth_max,
        i.fifo_depth_peak_at_enqueue,
        i.lag_p50_us,
        i.lag_p95_us,
        r.rto_fired,
        r.rto_spurious,
        r.rto_useful,
        r.transport.cwnd
    );
}

/// The FIFO never stands deeper than the gate allows.
///
/// `D_target + one datagram` is exact, not a tolerance: the gate admits a
/// datagram only while the FIFO holds less than `D_target`, and the check
/// precedes every send, so the deepest the queue can be right after an
/// accept is `D_target - 1 + one datagram`. `d_target_max` is the largest
/// `D_target` any pass used (it follows the rate, which follows cwnd), so the
/// bound is the loosest the run ever allowed. The reading is
/// `fifo_depth_peak_at_enqueue`, taken from quinn's own queue at the instant
/// each datagram was accepted, so it also sees what a post-drain sample
/// would miss.
///
/// Scheduling decides how often the gate is exercised, not whether it holds,
/// so this is the "structural property in a scene" kind of gate: the premises
/// below (the FIFO was used, the gate held work back, the run lost datagrams)
/// are what make it more than vacuous.
fn assert_fifo_bounded(n: &str, r: &BrowserlessResult) {
    let i = &r.transport.instr;
    let a = &i.admission;
    assert!(
        a.pumps > 0 && a.d_target_max > 0 && a.mtu > 0,
        "{n}: no admission pass ran"
    );
    let bound = a.d_target_max + a.mtu;
    // The bound first: with the gate removed the FIFO grows to megabytes and
    // this is the assertion that must say so, not a premise.
    assert!(
        i.fifo_depth_peak_at_enqueue <= bound,
        "{n}: the FIFO stood at {} B right after an accept; D_target max {} + one datagram {} = {bound}",
        i.fifo_depth_peak_at_enqueue,
        a.d_target_max,
        a.mtu
    );
    assert!(
        i.fifo_depth_max <= bound,
        "{n}: a post-drain reading of {} B exceeds {bound}",
        i.fifo_depth_max
    );
    assert_eq!(
        a.depth_violations, 0,
        "{n}: a datagram was accepted at or above D_target"
    );
    // The bound is only as strong as `D_target` is small. 10 ms of the rate
    // is 125 kB at 100 Mbit/s (design F1), and this scene's link is a
    // fraction of that: a `D_target` above it would mean the gate's own
    // arithmetic, which the bound above is stated in, had stopped bounding.
    assert!(
        a.d_target_max <= 125_000,
        "{n}: D_target reached {} B, more than 10 ms of 100 Mbit/s",
        a.d_target_max
    );
    // Premises.
    assert!(r.s2c_datagrams_dropped > 0, "{n}: the run lost nothing");
    assert!(
        i.fifo_depth_peak_at_enqueue > 0 && i.quinn_limited_turns > 0,
        "{n}: the FIFO was never used"
    );
    assert!(
        i.budget.gate_depth > 0,
        "{n}: the depth gate never held work back"
    );
    assert!(
        a.charged_bytes + a.soft_charged_bytes == i.offered.total(),
        "{n}: charged {} + {} != offered {}: a byte class is unbilled",
        a.charged_bytes,
        a.soft_charged_bytes,
        i.offered.total()
    );
}

/// P2a: 1 % loss, i.i.d. and Gilbert-Elliott (mean burst 8), three seeds each.
/// Before admission the same scenes stood the FIFO at 2.2-4.7 MB.
#[tokio::test(start_paused = true)]
async fn p2a_the_fifo_never_exceeds_its_gate_under_one_percent_loss() {
    const SECS: u64 = 8;
    let ge = GilbertElliott::from_rate_and_burst(0.01, BURST, 1.0).expect("feasible");
    for seed in [0xB0_B057_u64, 0xB0_B058, 0xB0_B059] {
        let iid = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| p.loss = 0.01))
            .await
            .expect("scene ran");
        let n = format!("iid1%/{seed:x}");
        assert_established(&n, &iid);
        report_instruments(&n, &iid);
        report_p2(&n, &iid);
        assert_fifo_bounded(&n, &iid);

        let burst = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| ge.apply_to(p)))
            .await
            .expect("scene ran");
        let n = format!("ge1%/{seed:x}");
        assert_established(&n, &burst);
        report_instruments(&n, &burst);
        report_p2(&n, &burst);
        assert_fifo_bounded(&n, &burst);
    }
}

/// P2b: quinn is never left idle while admission holds work and the tokens to
/// send it.
///
/// A saturating load over a link slower than the offered rate, so the
/// scheduler has a backlog on nearly every turn. `starved_turns` counts the
/// turns that ended with quinn's FIFO empty although admission held work and
/// a datagram's worth of tokens. It is zero because the admission pass
/// follows every `drain_outbound` (`refill_after_drain`); remove that and the
/// drain at the end of the turn empties the FIFO with nothing behind it.
///
/// Exact, not a margin: the scene is stepped, so the count is a function of
/// the seed, and the zero is structural.
#[tokio::test(start_paused = true)]
async fn p2b_quinn_is_never_left_idle_while_admission_holds_work() {
    use ghostframe_e2e::netsim::{Bottleneck, CapTimeline};
    for (name, cap, loss) in [
        ("saturated_2mbit", 250_000u64, 0.0),
        ("saturated_2mbit_loss1%", 250_000, 0.01),
    ] {
        let mut sc = scene(0xB0_B057, 8);
        sc.net = NetProfile {
            delay_us: 10_000,
            cap: CapTimeline::constant(cap),
            bottleneck: Some(Bottleneck::wifi()),
            loss,
            ..NetProfile::perfect()
        };
        let r = run_browserless_with_links(sc, LinkOverrides::default())
            .await
            .expect("scene ran");
        assert_established(name, &r);
        report_instruments(name, &r);
        report_p2(name, &r);
        let a = &r.transport.instr.admission;
        println!(
            "P2B {name}: s2c_bytes={} backlog_turns={} starved_turns={} pumps={}",
            r.bytes_delivered_s2c, a.backlog_turns, a.starved_turns, a.pumps
        );
        assert_eq!(
            a.starved_turns, 0,
            "{name}: quinn idle with work and tokens"
        );
        // Premise: admission really held work, on many turns, so a zero is
        // not the zero of a scene that never had a backlog.
        assert!(
            a.backlog_turns >= 100,
            "{name}: only {} backlogged turns",
            a.backlog_turns
        );
        assert!(r.transport.instr.budget.gate_depth > 0);
    }
}

// ---------------------------------------------------------------------------
// F4 gate (D1 PR-4): the retransmission timer is a measured probe timeout.
// ---------------------------------------------------------------------------

/// P4: under 2 % i.i.d. loss the share of RTO fires that were for data that
/// had in fact arrived (`rto_original_first`: the pass was first acknowledged
/// through its original, the Eifel test), pooled over five seeds, is at most
/// 10 %; and a clean link fires no timer at all.
///
/// Why i.i.d., why this counter, why pooled. `rto_spurious` takes a spurious
/// verdict back when the retransmission is acknowledged too, which it nearly
/// always is (every duplicate is acknowledged), so it read 0 on every base run
/// and could not tell a bad timer from a good one; the design's 0.68 to 0.75
/// was the retransmit loop of #225. `rto_original_first` is final. The scene is
/// stepped, so each seed's value is exact, but it is chaotic: a review fix that
/// changed only what happens around stalls moved one seed from 0.001 to 0.114
/// while the other four stayed under 0.05. A per-seed bound would gate on that
/// one draw; the pooled share (total originals-first over total fires) does
/// not. Per-seed values are printed.
///
/// | build | share per seed (sorted) | pooled |
/// |---|---|---|
/// | PR-3 base (p95 x 2, 150 ms floor) | 0.020 0.062 0.095 0.121 0.132 | 0.083 |
/// | this change | 0.004 0.008 0.026 0.043 0.114 | 0.040 |
/// | arm at enqueue, p95 x 2, 16 MiB buffer, `T_q` 500 ms | 0.667 0.878 0.887 0.905 0.935 | 0.89 |
///
/// The bound sits 2.5x above this change and 8.9x below the mutation, and the
/// mutation's best seed (0.667) is above it by 6x. It does not separate this
/// change from the base (0.083 pooled is already under it): what this
/// change buys over the base is the 0.083 to 0.040 reduction, reported not
/// gated. Burst loss (Gilbert-Elliott) is reported by the sweep but not gated:
/// its shares are 0 to 0.77 per seed under the same mutation.
///
/// Premises, asserted: each run fires at least 50 times (a share of nothing is
/// not a share), the session carried load, the netsim dropped something.
#[tokio::test(start_paused = true)]
async fn p4_spurious_share_of_rto_fires_under_iid_loss() {
    const SECS: u64 = 12;
    let (mut fired, mut original_first) = (0u64, 0u64);
    for i in 0..5u64 {
        let seed = 0xB0_B057 ^ (i * 0x100);
        let lossy = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| p.loss = RATE))
            .await
            .expect("scene ran");
        let n = format!("iid2%@{seed:#x}");
        assert_established(&n, &lossy);
        assert!(lossy.s2c_datagrams_dropped > 0, "{n}: dropped nothing");
        assert!(
            lossy.rto_fired >= 50,
            "{n}: premise: only {} RTO fires, a share of them proves nothing",
            lossy.rto_fired
        );
        let share = lossy.rto_original_first as f64 / lossy.rto_fired as f64;
        println!(
            "P4 {n}: rto_fired={} rto_original_first={} (rto_spurious={} rto_useful={}) share={share:.4}",
            lossy.rto_fired, lossy.rto_original_first, lossy.rto_spurious, lossy.rto_useful
        );
        fired += lossy.rto_fired;
        original_first += lossy.rto_original_first;
    }
    let pooled = original_first as f64 / fired as f64;
    println!("P4 pooled share over 5 seeds: {pooled:.4} ({original_first} of {fired})");
    assert!(
        pooled <= 0.10,
        "{original_first} of {fired} RTO fires were for data that arrived"
    );
    for i in 0..3u64 {
        let seed = 0xB0_B057 ^ (i * 0x100);
        let clean = run_browserless_with_links(scene(seed, SECS), LinkOverrides::default())
            .await
            .expect("scene ran");
        let n = format!("clean@{seed:#x}");
        assert_established(&n, &clean);
        assert_eq!(clean.s2c_datagrams_dropped, 0, "{n}: control dropped");
        assert!(
            clean.transport.instr.generation_replacements > 0,
            "{n}: premise: no tile changed, the control exercised nothing"
        );
        assert_eq!(
            clean.rto_fired, 0,
            "{n}: a timer fired on a link that dropped nothing"
        );
    }
}

// ---------------------------------------------------------------------------
// F4 part 2 gates (D1 PR-5): threshold loss drives repair, the probe is the tail.
// ---------------------------------------------------------------------------

/// One full-grid CDF 5/3 frame, then silence: everything the server owes is in
/// the first second, so whatever is still unrepaired at the end is stranded.
fn full_grid_frame() -> FrameScript {
    FrameScript {
        tiles: (0..COLS as u8)
            .flat_map(|x| {
                (0..ROWS as u8).map(move |y| {
                    (
                        (x, y),
                        TileSpec::Cdf53 {
                            bgra: gradient_tile(0, x, y),
                        },
                    )
                })
            })
            .collect(),
    }
}

fn p5_scene(seed: u64, secs: u64) -> BrowserlessScene {
    let mut sc = scene(seed, secs);
    sc.load = SceneLoad::Script(vec![full_grid_frame()]);
    sc
}

/// What one lossy run cost, from the counters that name the cause.
struct RepairBill {
    lost_datagrams: u64,
    retransmits: u64,
    by_loss: u64,
    by_probe: u64,
    by_nack: u64,
}

fn repair_bill(r: &BrowserlessResult) -> RepairBill {
    let i = &r.transport.instr;
    RepairBill {
        lost_datagrams: r.client_wire_loss.lost_pre_fec,
        retransmits: r.retransmit_attempts_total,
        by_loss: i.loss_retransmits,
        by_probe: i.pto_probe_retransmits,
        by_nack: r.retransmit_by_nack,
    }
}

/// P5a: a lossy screen is repaired, once. Over five seeds of 2 % i.i.d. loss
/// and of 2 % Gilbert-Elliott loss (mean burst 8), each on one full-grid CDF
/// 5/3 frame followed by silence:
///
/// - **Structural** (counters that must be exactly zero or all-present): every
///   tile is on the client's screen, the retransmit cache is empty at the end
///   (nothing sent was left without a repair that arrived), and nothing was
///   retired by the 30 s backstop. A transmission lost with current content
///   and never retransmitted would sit in the cache for ever.
/// - **Margin-argued**: retransmissions per datagram the client saw missing
///   (`lost_pre_fec`, datagram-level: one lost QUIC packet carries several)
///   is at most `RATIO_BOUND`.
///
/// Re-measured baselines on the sweep scene family (15 runs per row, sweep
/// `burst_loss_report_over_three_seeds`): the PR-3 base 0.86, PR-4 1.03 (RTO
/// and NACK both repair the same loss). The design's 2.7 to 26 was the #225
/// loop and the unbounded FIFO.
const RATIO_BOUND: f64 = 1.2;

#[tokio::test(start_paused = true)]
async fn p5a_a_lost_datagram_is_repaired_about_once_and_nothing_is_stranded() {
    const P5_RATE: f64 = 0.05;
    let ge = GilbertElliott::from_rate_and_burst(P5_RATE, BURST, 1.0).expect("feasible");
    let (mut lost, mut retx, mut loss_repairs) = (0u64, 0u64, 0u64);
    for i in 0..5u64 {
        let seed = 0xB0_B057 ^ (i * 0x100);
        for (model, links) in [
            ("iid5%", s2c_only(|p| p.loss = P5_RATE)),
            ("burst5%", s2c_only(|p| ge.apply_to(p))),
        ] {
            let r = run_browserless_with_links(p5_scene(seed, 10), links)
                .await
                .expect("scene ran");
            let n = format!("{model}@{seed:#x}");
            assert_established(&n, &r);
            let b = repair_bill(&r);
            println!(
                "P5A {n}: lost_datagrams={} retransmits={} (loss={} probe={} nack={}) ratio={:.3} \
                 cache_left={} tiles={} stale={} verdicts={} spurious={} k_pkt={} pto_fires={} \
                 critical_lat_mean_us={} max_us={}",
                b.lost_datagrams,
                b.retransmits,
                b.by_loss,
                b.by_probe,
                b.by_nack,
                b.retransmits as f64 / b.lost_datagrams.max(1) as f64,
                r.emitter_cache_entries,
                tiles_present(&r),
                r.stale_generation_tiles,
                r.transport.instr.loss_by_packet_threshold
                    + r.transport.instr.loss_by_time_threshold,
                r.transport.instr.ledger_spurious_losses,
                r.transport.instr.k_pkt,
                r.transport.instr.pto_fires,
                r.critical_latency_mean_us,
                r.critical_latency_max_us,
            );
            loss_repairs += b.by_loss;
            assert_eq!(
                tiles_present(&r),
                (COLS * ROWS) as usize,
                "{n}: tiles missing at the end"
            );
            assert_eq!(
                r.emitter_cache_entries, 0,
                "{n}: passes left in the retransmit cache: sent, never acknowledged"
            );
            assert_eq!(r.transport.instr.loss_by_pto, 0, "{n}");
            lost += b.lost_datagrams;
            retx += b.retransmits;
        }
    }
    let ratio = retx as f64 / lost as f64;
    println!("P5A pooled: retransmits={retx} lost_datagrams={lost} ratio={ratio:.3}");
    assert!(lost >= 500, "premise: only {lost} missing datagrams");
    // Who repaired it. The client's own gap NACKs also repair a loss they
    // can see, so with the threshold detector switched off the ratio and the
    // latency below do not move (measured: ratio 0.968, the same latencies);
    // what moves is the cause: no repair at all comes from the detector, and
    // the client's NACKs and the probe do the work. Measured with it on: 1302
    // of 1445 retransmissions (90 %) are the detector's; off, 0.
    assert!(
        loss_repairs * 100 >= retx * 80,
        "the threshold detector made {loss_repairs} of {retx} retransmissions"
    );
    assert!(
        ratio <= RATIO_BOUND,
        "{retx} retransmissions for {lost} missing datagrams: {ratio:.3} > {RATIO_BOUND}"
    );
}

/// The tail: the ONLY datagram of the screen is lost and the screen then stays
/// static (one Solid tile, one datagram, dropped once). Nothing after it will
/// ever be acknowledged, so no threshold can find it, and the client cannot ask
/// for a pass it never heard of. The probe timeout must repair it, with no frame
/// and no heartbeat that matters to wake the server: about one PTO after the
/// send, which before any acknowledgement is RFC 9002's initial 1004 ms.
///
/// Premises: the drop fired and hit the only datagram (the tile appears in no
/// other way: no verdict, no NACK). Outcome: one probe retransmission, the tile
/// on screen, the cache empty, and the repair neither early (before the PTO)
/// nor at the 30 s backstop.
#[tokio::test(start_paused = true)]
async fn p5_a_lost_only_datagram_on_a_static_screen_is_repaired_by_the_probe_on_time() {
    let mut sc = scene(0xB0_B057, 6);
    sc.load = SceneLoad::Script(vec![FrameScript {
        tiles: vec![(
            (1, 1),
            TileSpec::Solid {
                bgra: [10, 20, 30, 255],
            },
        )],
    }]);
    sc.drops = DropPlan::new(vec![DropRule {
        tile_x: 1,
        tile_y: 1,
        occurrences: vec![0],
    }]);
    let r = run_browserless_with_links(sc, LinkOverrides::default())
        .await
        .expect("scene ran");
    let i = &r.transport.instr;
    println!(
        "P5TAIL drops_fired={:?} pto_fires={} probe_retx={} loss_retx={} nack_retx={} cache_left={}          tile_present={} queued_critical_lat_us={}",
        r.drops_fired,
        i.pto_fires,
        i.pto_probe_retransmits,
        i.loss_retransmits,
        r.retransmit_by_nack,
        r.emitter_cache_entries,
        r.framebuffer.tile_rgba(1, 1).is_some(),
        r.queued_critical_latency_mean_us
    );
    assert_eq!(
        r.drops_fired,
        vec![1],
        "premise: the only datagram was dropped"
    );
    assert_eq!(
        (i.loss_retransmits, r.retransmit_by_nack),
        (0, 0),
        "premise: nothing but the probe could have found it"
    );
    assert!(
        r.framebuffer.tile_rgba(1, 1).is_some(),
        "the lost only datagram was never repaired"
    );
    assert_eq!(i.pto_probe_retransmits, 1);
    assert_eq!(r.emitter_cache_entries, 0);
    // On time: the acknowledgement of the retransmission arrives one PTO plus a
    // round trip after the send. Not before the PTO, not seconds after it.
    assert!(
        (1_000_000..1_300_000).contains(&r.queued_critical_latency_mean_us),
        "repaired {} us after the send; the PTO is 1004 ms",
        r.queued_critical_latency_mean_us
    );
}
