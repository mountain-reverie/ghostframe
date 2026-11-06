//! P5b: a recorded `NetSim::decide` trace replayed into the loss detector.
//!
//! No sockets, no harness, no clock: the datagrams of a fixed-seed netsim run
//! (delivery times from `NetSim::decide`, reordering included) are turned into
//! the acknowledgement order the server would see, and fed to the
//! `TransmissionLedger` exactly as the bridge feeds it: record and stamp the
//! departure, resolve on arrival, ask for verdicts after every
//! acknowledgement. The trace is a function of the seed, so every number below
//! is exact.
//!
//! Two traces:
//!
//! - **Adjacent swaps.** `reorder_us` of 1.46 send intervals makes one datagram
//!   overtake the one before it with probability `(1 - 1/1.46)^2 / 2`, about
//!   0.05, and never by more than one place. A packet threshold of 3 cannot
//!   fire on that: zero verdicts, which is the point of 3.
//! - **Deep reordering.** A reorder window of 8 intervals displaces datagrams by
//!   up to 7 places, past the initial threshold. The detector's verdicts are
//!   then contradicted by the late acknowledgements; the adaptive threshold
//!   raises `K_PKT` until the detector stops being wrong, and the spurious
//!   verdicts per datagram over the last quarter of the trace fall under 0.05.
//!   With `K_PKT` frozen at 3 (the mutation) they stay far above it.

use std::time::{Duration, Instant};

use ghostframe_e2e::netsim::{NetProfile, NetSim, Verdict};
use ghostframe_lib::transport::reliable_emitter::EmitKey;
use ghostframe_lib::transport::transmission_ledger::{
    ReoBranch, Resolution, Transmission, TransmissionLedger,
};

const INTERVAL_US: u64 = 1_000;
const N: u32 = 20_000;

/// Delivery times of `N` datagrams sent `INTERVAL_US` apart through a netsim
/// with this profile, in send order. `None`: dropped.
fn trace(profile: NetProfile, seed: u64) -> Vec<Option<u64>> {
    trace_shaped(profile, seed, Shape::Uniform)
}

/// How the datagrams are spaced in time.
#[derive(Clone, Copy)]
enum Shape {
    /// One every `INTERVAL_US`.
    Uniform,
    /// Frames: `BURST_LEN` datagrams `BURST_GAP_US` apart every
    /// `BURST_PERIOD_US`; the whole frame shares one departure stamp (one
    /// `drain_outbound` turn).
    Bursts,
}
const BURST_LEN: u32 = 40;
const BURST_GAP_US: u64 = 100;
const BURST_PERIOD_US: u64 = 33_000;

impl Shape {
    fn send_us(self, i: u32) -> u64 {
        match self {
            Shape::Uniform => i as u64 * INTERVAL_US,
            Shape::Bursts => {
                (i / BURST_LEN) as u64 * BURST_PERIOD_US + (i % BURST_LEN) as u64 * BURST_GAP_US
            }
        }
    }
    fn stamp_us(self, i: u32) -> u64 {
        match self {
            Shape::Uniform => i as u64 * INTERVAL_US,
            Shape::Bursts => (i / BURST_LEN) as u64 * BURST_PERIOD_US,
        }
    }
}

fn trace_shaped(profile: NetProfile, seed: u64, shape: Shape) -> Vec<Option<u64>> {
    let mut sim = NetSim::new(profile, seed);
    (0..N)
        .map(|i| match sim.decide(1200, shape.send_us(i)) {
            Verdict::Deliver { at_us }
            | Verdict::Duplicate { at_us, .. }
            | Verdict::Corrupt { at_us, .. } => Some(at_us),
            Verdict::Drop => None,
        })
        .collect()
}

#[derive(Debug, Default)]
struct Replay {
    verdicts: u64,
    /// Datagrams declared lost and acknowledged afterwards.
    spurious: u64,
    /// The same, among the datagrams of the last quarter of the trace.
    spurious_last_quarter: u64,
    /// Late acknowledgements the ledger took for FEC recoveries (not retracted,
    /// not counted as reordering). The trace has no parity loss recovery in it,
    /// so every one is a misreading.
    taken_for_fec: u64,
    verdicts_last_quarter: u64,
    k_pkt_at_end: u64,
    reo_wnd_at_end_us: u64,
    reo_wnd_raises: u64,
    reo_learned_packet: u64,
    reo_learned_time: u64,
    /// Datagrams the netsim dropped and the detector found.
    real_losses_found: u64,
    real_losses: u64,
    /// Mean and maximum time from departure to verdict over the real losses
    /// found, in microseconds.
    real_loss_latency_mean_us: u64,
    real_loss_latency_max_us: u64,
    /// Which rule judged the real losses found: the count rule outside the
    /// window, inside it at the learned depth, inside it once the stream went
    /// quiet, or the clock.
    real_by_open: u64,
    real_by_depth: u64,
    real_by_quiet: u64,
    real_by_time: u64,
    reo_depth_at_end: u64,
    reo_stall_skips: u64,
}

fn replay(trace: &[Option<u64>], freeze: bool) -> Replay {
    // These traces are about the count threshold; the reordering window has
    // its own tests below.
    replay_shaped(trace, Shape::Uniform, freeze, true, 0)
}

fn replay_shaped(
    trace: &[Option<u64>],
    shape: Shape,
    freeze: bool,
    freeze_reo_wnd: bool,
    // Acknowledgements are delivered in batches this many microseconds long
    // (0: one by one, each followed by a verdict query). The client sends one
    // about every 5 ms and the bridge resolves the whole batch before it
    // asks for verdicts, which inflates a displacement.
    batch_us: u64,
) -> Replay {
    let t0 = Instant::now();
    let mut ledger = TransmissionLedger::new(65_536);
    if freeze {
        ledger.freeze_k_pkt();
    }
    if freeze_reo_wnd {
        ledger.freeze_reo_wnd();
    }
    // Acknowledgements in delivery order: (delivery, arrival, ws).
    let mut arrivals: Vec<(u64, u64, u32)> = trace
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            t.map(|t| {
                let d = if batch_us == 0 {
                    t
                } else {
                    t.div_ceil(batch_us) * batch_us
                };
                (d, t, i as u32)
            })
        })
        .collect();
    arrivals.sort();
    let mut out = Replay::default();
    let mut declared = std::collections::HashSet::new();
    let mut latency_sum = 0u64;
    // 9/8 of a round trip that includes the whole reorder window.
    let threshold = Duration::from_millis(60);
    let quarter = N * 3 / 4;
    let tally = |out: &mut Replay,
                 declared: &mut std::collections::HashSet<u32>,
                 latency_sum: &mut u64,
                 fired: Vec<(
        u32,
        Transmission,
        ghostframe_lib::transport::transmission_ledger::LossReason,
    )>,
                 branches: Vec<(u32, ReoBranch)>,
                 at_us: u64| {
        for (lost_ws, _, _) in fired {
            if trace[lost_ws as usize].is_none() {
                match branches.iter().find(|(w, _)| *w == lost_ws) {
                    Some((_, ReoBranch::Open)) => out.real_by_open += 1,
                    Some((_, ReoBranch::Depth)) => out.real_by_depth += 1,
                    Some((_, ReoBranch::Quiet)) => out.real_by_quiet += 1,
                    None => out.real_by_time += 1,
                }
                let lat = at_us.saturating_sub(shape.stamp_us(lost_ws));
                *latency_sum += lat;
                out.real_loss_latency_max_us = out.real_loss_latency_max_us.max(lat);
            }
            out.verdicts += 1;
            declared.insert(lost_ws);
            if lost_ws >= quarter {
                out.verdicts_last_quarter += 1;
            }
        }
    };
    // Departures happen in send order, one per datagram, interleaved with the
    // arrivals in time: a datagram has departed only once its send time has
    // come (the ledger's FEC-distance rule reads how many departures followed).
    let mut next_send = 0u32;
    // The ordinary round trip, here the one-way delay: the replay returns the
    // acknowledgement at once.
    let min_rtt = Duration::from_micros(20_000);
    let mut g = 0usize;
    while g < arrivals.len() {
        // One delivery: a batch, or a single acknowledgement.
        let mut end = g + 1;
        if batch_us > 0 {
            while end < arrivals.len() && arrivals[end].0 == arrivals[g].0 {
                end += 1;
            }
        }
        let at_us = arrivals[g].0;
        while next_send < N && shape.send_us(next_send) <= at_us {
            let i = next_send;
            let at = t0 + Duration::from_micros(shape.stamp_us(i));
            ledger.record(
                i,
                at,
                Transmission {
                    emit_us: shape.send_us(i),
                    wire_bytes: 1200,
                    content: EmitKey::new(i, 0, 0, 0).into(),
                    depart_us: None,
                    dep_idx: None,
                },
            );
            ledger.mark_departed(i, at, shape.send_us(i));
            // The group's parity leaves right behind the datagram that
            // allocates `first + 20`, i.e. after `first + 19` has left.
            if i % 10 == 9 && i >= 19 {
                ledger.note_parity_departed(i - 19, at, Duration::from_millis(600));
            }
            next_send += 1;
        }
        let now = t0 + Duration::from_micros(at_us);
        // The bridge's loss timer: fires at `next_loss_time` between
        // acknowledgements, as the event loop does.
        while let Some(due) = ledger.next_loss_time(threshold) {
            if due > now {
                break;
            }
            let fired = ledger.detect_losses_at(due, threshold);
            if fired.is_empty() {
                break;
            }
            let due_us = due.duration_since(t0).as_micros() as u64;
            let branches = ledger.take_verdict_branches();
            tally(
                &mut out,
                &mut declared,
                &mut latency_sum,
                fired,
                branches,
                due_us,
            );
        }
        for &(_, _, ws) in &arrivals[g..end] {
            match ledger.resolve_at(ws, Some((now, min_rtt))) {
                Some(Resolution::Late { fec_possible, .. }) => {
                    out.spurious += 1;
                    out.taken_for_fec += u64::from(fec_possible);
                    if ws >= quarter {
                        out.spurious_last_quarter += 1;
                    }
                }
                Some(Resolution::Live(_)) | None => {}
            }
        }
        let fired = ledger.detect_losses_at(now, threshold);
        let branches = ledger.take_verdict_branches();
        tally(
            &mut out,
            &mut declared,
            &mut latency_sum,
            fired,
            branches,
            at_us,
        );
        g = end;
    }
    out.real_losses = trace.iter().filter(|t| t.is_none()).count() as u64;
    out.real_losses_found = (0..N)
        .filter(|i| trace[*i as usize].is_none() && declared.contains(i))
        .count() as u64;
    out.real_loss_latency_mean_us = latency_sum / out.real_losses_found.max(1);
    out.k_pkt_at_end = ledger.k_pkt();
    out.reo_wnd_at_end_us = ledger.reo_wnd().as_micros() as u64;
    out.reo_wnd_raises = ledger.stats().reo_wnd_raises;
    out.reo_learned_packet = ledger.stats().reo_learned_packet;
    out.reo_learned_time = ledger.stats().reo_learned_time;
    out.reo_depth_at_end = ledger.stats().reo_depth;
    out.reo_stall_skips = ledger.stats().reo_stall_skips;
    out
}

fn profile(reorder_intervals: f64, loss: f64) -> NetProfile {
    NetProfile {
        delay_us: 20_000,
        reorder_us: (reorder_intervals * INTERVAL_US as f64) as u64,
        loss,
        ..NetProfile::perfect()
    }
}

#[test]
fn adjacent_swaps_never_reach_the_packet_threshold() {
    let t = trace(profile(1.46, 0.0), 0x5EED);
    // Premise: the trace really contains swaps, at about p = 0.05, and none
    // deeper than one place.
    let mut order: Vec<(u64, u32)> = t
        .iter()
        .enumerate()
        .map(|(i, t)| (t.unwrap(), i as u32))
        .collect();
    order.sort();
    let swaps = order.windows(2).filter(|w| w[0].1 > w[1].1).count();
    let max_depth = order
        .iter()
        .enumerate()
        .map(|(pos, (_, ws))| (*ws as i64 - pos as i64).unsigned_abs())
        .max()
        .unwrap();
    let p = swaps as f64 / N as f64;
    println!("P5B adjacent: swaps={swaps} p={p:.4} max_depth={max_depth}");
    assert!((0.03..0.08).contains(&p), "premise: p = {p}");
    assert!(max_depth <= 1, "premise: depth {max_depth}");

    let r = replay(&t, false);
    println!("P5B adjacent: {r:?}");
    assert_eq!(
        r.verdicts, 0,
        "adjacent swaps must not look like loss: {r:?}"
    );
    assert_eq!(r.k_pkt_at_end, 3);
}

/// 2 % real loss under deep reordering. Exact for the fixed seed.
#[test]
fn the_adaptive_threshold_stops_being_wrong_about_deep_reordering() {
    let t = trace(profile(8.0, 0.02), 0x5EED);
    let adaptive = replay(&t, false);
    let frozen = replay(&t, true);
    let per_dg = |n: u64| n as f64 / (N / 4) as f64;
    println!(
        "P5B deep: adaptive {adaptive:?} spurious_last_quarter_per_datagram={:.4}",
        per_dg(adaptive.spurious_last_quarter)
    );
    println!(
        "P5B deep: frozen   {frozen:?} spurious_last_quarter_per_datagram={:.4}",
        per_dg(frozen.spurious_last_quarter)
    );
    // Premises: the reordering really is deeper than the initial threshold
    // (the frozen detector is wrong often), losses are really in the trace, and
    // the adaptive one raised its threshold.
    assert!(
        per_dg(frozen.spurious_last_quarter) > 0.05,
        "premise: K_PKT = 3 is not wrong enough on this trace to test anything: {frozen:?}"
    );
    assert!(adaptive.real_losses > 100, "premise: {adaptive:?}");
    assert!(adaptive.k_pkt_at_end > 3, "premise: {adaptive:?}");
    // The gate.
    assert!(
        per_dg(adaptive.spurious_last_quarter) <= 0.05,
        "spurious verdicts per datagram over the last quarter: {:.4}: {adaptive:?}",
        per_dg(adaptive.spurious_last_quarter)
    );
    // And it still finds the real losses once it has adapted: at least 90 % of
    // the datagrams the netsim dropped were declared lost.
    assert!(
        adaptive.real_losses_found * 10 >= adaptive.real_losses * 9,
        "real losses found {} of {}",
        adaptive.real_losses_found,
        adaptive.real_losses
    );
}

/// Reordering as deep as the interleave itself: a window of 16 intervals
/// displaces a datagram by up to 15 places. A single group member delayed by
/// 19 - p intervals or more (p its position in the group) acknowledges exactly
/// when a parity recovery of it would, and nothing the server can see tells the
/// two apart without a wire flag, so some of those are taken for recoveries.
/// What the group-aware test does buy is the case of several members of one
/// group displaced together (two cannot both be rebuilt), which halves the
/// misreading here.
///
/// What is asserted is the property, not the number: the misread share stays
/// a minority (the sibling test removed, it is 40 %), and the late
/// acknowledgements that are not taken for recoveries still drive the
/// threshold to its ceiling. Exact for the fixed seed; see the comment on
/// `Provenance::Rebuilt` in the PR body for the wire-flag follow-up that
/// closes the rest.
#[test]
fn deep_reordering_is_mostly_not_mistaken_for_parity_recovery() {
    let t = trace(profile(16.0, 0.02), 0x5EED);
    let adaptive = replay(&t, false);
    println!("P5B 16-interval: {adaptive:?}");
    // Premises: late acknowledgements exist in quantity, and there are
    // real losses in the trace.
    assert!(adaptive.spurious > 200, "premise: {adaptive:?}");
    assert!(adaptive.real_losses > 100, "premise: {adaptive:?}");
    assert!(
        adaptive.taken_for_fec * 4 <= adaptive.spurious,
        "late acknowledgements taken for parity recoveries: {} of {}",
        adaptive.taken_for_fec,
        adaptive.spurious
    );
    assert!(adaptive.k_pkt_at_end > 3, "adapts: {adaptive:?}");
}

/// Frames leave in bursts of 40 datagrams 100 us apart, every 33 ms, and an 8 ms
/// reordering window moves datagrams tens of places inside a burst: past
/// `K_PKT_MAX` however far the count adapts. What helps is time: a datagram is
/// judged lost by the packet rule only if the newest acknowledged departure
/// left `reo_wnd` after it, or, inside a burst (one departure stamp), once the
/// stream of later departures has been quiet for `reo_wnd`. With 2 % real loss.
/// Exact for the fixed seed.
///
/// Before the in-burst rule, every real loss waited for the next frame's
/// acknowledgement: mean verdict latency 53.7 ms against 29.9 ms for the
/// detector without a window (+80 %). The latency gate is the other half of
/// the spurious-verdict gate: a window that "tolerates" reordering by never
/// judging passes the first alone.
#[test]
fn bursty_reordering_is_tolerated_by_the_reordering_window() {
    bursty_case(0);
}

/// The same with the acknowledgements delivered in batches of about 5 ms, as
/// the client sends them, and the whole batch resolved before verdicts are
/// asked for: a frame that arrives in one batch shows up as a displacement of
/// its full length (up to 40), which used to switch the in-window count rule
/// off for the session.
#[test]
fn bursty_batched_acknowledgements_do_not_cost_the_window_its_count_rule() {
    bursty_case(5_000);
}

fn bursty_case(batch_us: u64) {
    let t = trace_shaped(profile(8.0, 0.02), 0x5EED, Shape::Bursts);
    let with = replay_shaped(&t, Shape::Bursts, false, false, batch_us);
    let without = replay_shaped(&t, Shape::Bursts, false, true, batch_us);
    let per_dg = |n: u64| n as f64 / (N / 4) as f64;
    println!(
        "P5B bursts (batch {batch_us} us): with reo_wnd {with:?} spurious_last_quarter_per_datagram={:.4}",
        per_dg(with.spurious_last_quarter)
    );
    println!(
        "P5B bursts (batch {batch_us} us): frozen reo_wnd {without:?} spurious_last_quarter_per_datagram={:.4}",
        per_dg(without.spurious_last_quarter)
    );
    println!(
        "P5B bursts: real-loss verdict latency mean {} us (max {} us) with the window, \
         {} us (max {} us) without: x{:.2}",
        with.real_loss_latency_mean_us,
        with.real_loss_latency_max_us,
        without.real_loss_latency_mean_us,
        without.real_loss_latency_max_us,
        with.real_loss_latency_mean_us as f64 / without.real_loss_latency_mean_us as f64
    );
    // Premises: the unprotected detector is wrong often on this trace, real
    // losses are in it, and the window was learned.
    assert!(
        per_dg(without.spurious_last_quarter) > 0.05,
        "premise: no reordering window is not wrong enough to test anything: {without:?}"
    );
    assert!(with.real_losses > 100, "premise: {with:?}");
    assert!(with.reo_wnd_raises > 0, "premise: {with:?}");
    // ... from contradicted packet verdicts (the other kind is the clock's).
    assert!(with.reo_learned_packet > 0, "premise: {with:?}");
    assert_eq!(without.reo_wnd_raises, 0);
    // The gate.
    assert!(
        per_dg(with.spurious_last_quarter) <= 0.05,
        "spurious verdicts per datagram over the last quarter: {:.4}: {with:?}",
        per_dg(with.spurious_last_quarter)
    );
    // One burst of reordering is not sixteen: the window is learned in a few
    // steps and keeps (it used to be relearned about fifteen times, each
    // relearning a burst of spurious verdicts).
    // (With batched acknowledgements one more: the first batch teaches a gap
    // one step short.)
    let max_raises = if batch_us == 0 { 3 } else { 4 };
    assert!(
        with.reo_wnd_raises <= max_raises,
        "the window was raised {} times: {with:?}",
        with.reo_wnd_raises
    );
    // Which rule judged the real losses. The displacement here (39) is past
    // the old depth limit of 32, which switched the in-window count off; the
    // depth must be learned and kept, and the in-window losses are found by
    // the stream going quiet, the rest by the ordinary count outside the
    // window. None is the clock's: this trace has no tail short of the count.
    println!(
        "P5B bursts (batch {batch_us} us): real losses judged: open {} depth {} quiet {} time {}; \
         reo_depth {}",
        with.real_by_open,
        with.real_by_depth,
        with.real_by_quiet,
        with.real_by_time,
        with.reo_depth_at_end
    );
    assert!(with.reo_depth_at_end > 32, "premise: {with:?}");
    assert!(with.reo_depth_at_end <= 64, "the depth saturates: {with:?}");
    assert_eq!(
        with.real_by_open + with.real_by_depth + with.real_by_quiet + with.real_by_time,
        with.real_losses_found,
        "every real loss found has a branch: {with:?}"
    );
    assert!(
        with.real_by_quiet * 2 > with.real_losses_found,
        "the stream going quiet judged most real losses: {with:?}"
    );
    assert_eq!(with.real_by_time, 0, "{with:?}");
    assert_eq!(with.reo_stall_skips, 0, "no stall in this trace: {with:?}");
    // The window must not cost real-loss detection: at least 90 % of the real
    // losses are found, and the mean verdict latency stays within a third of
    // the unprotected detector's (which "finds" them among many wrong
    // verdicts). Measured 1.31; it was 1.80 before the in-burst rule.
    assert!(
        with.real_losses_found * 100 >= with.real_losses * 90,
        "real losses found {} of {}",
        with.real_losses_found,
        with.real_losses
    );
    assert!(
        with.real_loss_latency_mean_us * 100 <= without.real_loss_latency_mean_us * 135,
        "real-loss latency {} us against {} us without the window",
        with.real_loss_latency_mean_us,
        without.real_loss_latency_mean_us
    );
}

/// A path that reorders by one place at most never opens the window.
#[test]
fn uniform_traces_without_deep_reordering_keep_the_window_closed() {
    let t = trace(profile(1.46, 0.02), 0x5EED);
    let r = replay(&t, false);
    println!("P5B adjacent+loss: {r:?}");
    assert!(r.real_losses > 100, "premise: {r:?}");
    assert_eq!(r.reo_wnd_raises, 0, "{r:?}");
    assert_eq!(r.reo_wnd_at_end_us, 0, "{r:?}");
}

/// Deep uniform reordering with the count threshold frozen at 3: the departure-
/// gap window alone stops the spurious verdicts that the frozen count makes
/// (the premise: with both frozen they are common).
#[test]
fn the_reordering_window_alone_tolerates_deep_uniform_reordering() {
    let t = trace(profile(8.0, 0.02), 0x5EED);
    let window_only = replay_shaped(&t, Shape::Uniform, true, false, 0);
    let neither = replay_shaped(&t, Shape::Uniform, true, true, 0);
    let per_dg = |n: u64| n as f64 / (N / 4) as f64;
    println!("P5B deep, window only: {window_only:?}");
    println!("P5B deep, neither:     {neither:?}");
    assert!(
        per_dg(neither.spurious_last_quarter) > 0.05,
        "premise: {neither:?}"
    );
    assert_eq!(window_only.k_pkt_at_end, 3, "premise: the count stayed put");
    assert!(window_only.reo_wnd_raises > 0, "premise: {window_only:?}");
    assert!(
        per_dg(window_only.spurious_last_quarter) <= 0.05,
        "{window_only:?}"
    );
    assert!(
        window_only.real_losses_found * 100 >= window_only.real_losses * 98,
        "{window_only:?}"
    );
}
