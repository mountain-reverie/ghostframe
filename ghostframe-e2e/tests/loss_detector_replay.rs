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
    Resolution, Transmission, TransmissionLedger,
};

const INTERVAL_US: u64 = 1_000;
const N: u32 = 20_000;

/// Delivery times of `N` datagrams sent `INTERVAL_US` apart through a netsim
/// with this profile, in send order. `None`: dropped.
fn trace(profile: NetProfile, seed: u64) -> Vec<Option<u64>> {
    let mut sim = NetSim::new(profile, seed);
    (0..N)
        .map(|i| match sim.decide(1200, i as u64 * INTERVAL_US) {
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
    /// Datagrams the netsim dropped and the detector found.
    real_losses_found: u64,
    real_losses: u64,
}

fn replay(trace: &[Option<u64>], freeze: bool) -> Replay {
    let t0 = Instant::now();
    let mut ledger = TransmissionLedger::new(65_536);
    if freeze {
        ledger.freeze_k_pkt();
    }
    // Acknowledgements in arrival order (stable for equal times).
    let mut arrivals: Vec<(u64, u32)> = trace
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.map(|t| (t, i as u32)))
        .collect();
    arrivals.sort();
    let mut out = Replay::default();
    let mut declared = std::collections::HashSet::new();
    // 9/8 of a round trip that includes the whole reorder window.
    let threshold = Duration::from_millis(60);
    let quarter = N * 3 / 4;
    // Departures happen in send order, one per datagram, interleaved with the
    // arrivals in time: a datagram has departed only once its send time has
    // come (the ledger's FEC-distance rule reads how many departures followed).
    let mut next_send = 0u32;
    // The ordinary round trip, here the one-way delay: the replay returns the
    // acknowledgement at once.
    let min_rtt = Duration::from_micros(20_000);
    for (at_us, ws) in arrivals {
        while next_send < N && next_send as u64 * INTERVAL_US <= at_us {
            let i = next_send;
            let at = t0 + Duration::from_micros(i as u64 * INTERVAL_US);
            ledger.record(
                i,
                at,
                Transmission {
                    emit_us: i as u64 * INTERVAL_US,
                    wire_bytes: 1200,
                    key: EmitKey::new(i, 0, 0, 0),
                    depart_us: None,
                    dep_idx: None,
                },
            );
            ledger.mark_departed(i, at, i as u64 * INTERVAL_US);
            // The group's parity leaves right behind the datagram that
            // allocates `first + 20`, i.e. after `first + 19` has left.
            if i % 10 == 9 && i >= 19 {
                ledger.note_parity_departed(i - 19, at);
            }
            next_send += 1;
        }
        let now = t0 + Duration::from_micros(at_us);
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
        for (lost_ws, _, _) in ledger.detect_losses_at(now, threshold) {
            out.verdicts += 1;
            declared.insert(lost_ws);
            if lost_ws >= quarter {
                out.verdicts_last_quarter += 1;
            }
        }
    }
    out.real_losses = trace.iter().filter(|t| t.is_none()).count() as u64;
    out.real_losses_found = (0..N)
        .filter(|i| trace[*i as usize].is_none() && declared.contains(i))
        .count() as u64;
    out.k_pkt_at_end = ledger.k_pkt();
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
