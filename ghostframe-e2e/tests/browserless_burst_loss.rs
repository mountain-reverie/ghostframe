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
use ghostframe_e2e::harness::browserless::{
    run_browserless_with_links, BrowserlessResult, BrowserlessScene, LinkOverrides, SceneLoad,
};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::netsim::{GilbertElliott, NetProfile};

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
         late_unreported={} unknown_acks={} tombstone_evictions={}] \
         ecn[ect0={} unmarked={} last_marked={}] rto_fired={} rto_spurious={} rto_useful={}",
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
        i.ecn.ect0_tx,
        i.ecn.unmarked_tx,
        i.ecn.last_marked,
        r.rto_fired,
        r.rto_spurious,
        r.rto_useful,
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
/// - `budget.pacer`: binds in some runs only (the pacer is 2.5x the estimate);
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
    // Departure mirror: every datagram that went through the send path is
    // either stamped or still queued; only the control datagrams that bypass
    // the mirror (pongs, eviction) may be missing.
    let all: u64 = i.offered.datagrams.iter().sum();
    let tracked = all - i.offered.datagrams[W::Control.index()];
    assert!(i.lag_samples > 0, "{n}: no departure was stamped");
    assert!(
        tracked <= i.lag_samples + i.mirror_len && i.lag_samples + i.mirror_len <= all,
        "{n}: mirror lost track of datagrams: stamped {} + queued {} vs {tracked}..{all}",
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
        assert!(
            i.late_acked_before_flush > 0,
            "{n}: no late ack withdrew a pending loss"
        );
        assert!(
            i.lag_p50_us > 1_000,
            "{n}: a standing queue shows in the lag"
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
    const BUFFER: usize = 32 * 1024;
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
        ("saturated_4mbit", 500_000u64, 0.0),
        ("saturated_4mbit_loss1%", 500_000, 0.01),
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
