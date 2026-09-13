//! Tier-1 bandwidth-estimator bench.
//!
//! Drives the estimator against a simulated bottleneck: packets serialise at
//! the link rate, so offering more than capacity makes one-way delay grow —
//! the signal a delay-gradient controller is built to detect.
//!
//! Deterministic. Unlike the browserless scenes this is a pure function of
//! its inputs, so any failure here reproduces exactly from the source.

use ghostframe_lib::transport::bwe::{AckArrival, BweWrapper};
use ghostframe_lib::transport::io_bridge::{
    combine_pacing_budget, pacer_tick_budget_bytes, select_pacing_mode, PacingMode,
};
use std::time::{Duration, Instant};

const PKT_BYTES: u32 = 1200;
/// Mirrors `io_bridge::SCHEDULER_TICK_INTERVAL_US` (33.3 ms, 30 fps). Not
/// importable directly (private to that module), so pinned here as a
/// literal — the pacing-budget tests below only need *a* tick length
/// consistent between the quinn/AIMD side and the goog_cc side of the
/// comparison, not this exact production value.
const TICK_INTERVAL_US: f64 = 33_333.0;

/// Run `steps` 20 ms ticks against `capacity_bps(step)`, returning the
/// estimate after each tick.
fn run(steps: u32, capacity_bps: impl Fn(u32) -> u64) -> Vec<u64> {
    let t0 = Instant::now();
    let mut w = BweWrapper::new(1_000_000, t0);
    let mut out = Vec::with_capacity(steps as usize);

    let mut send_rate_bps: u64 = 1_000_000;
    let mut emit_us: u64 = 0;
    let mut queue_free_us: u64 = 0;

    for step in 0..steps {
        let capacity = capacity_bps(step).max(1);
        let bytes_this_tick = (send_rate_bps / 8) * 20 / 1000;
        let n = (bytes_this_tick / PKT_BYTES as u64).max(1);

        let mut batch = Vec::with_capacity(n as usize);
        for i in 0..n {
            // Spread sends across the tick, as a pacer would.
            let spacing_us = ((PKT_BYTES as u64 * 8 * 1_000_000) / send_rate_bps.max(1)).max(1);
            let send_us = emit_us + spacing_us * i;
            let serial_us = (PKT_BYTES as u64 * 8 * 1_000_000) / capacity;
            let start_us = queue_free_us.max(send_us);
            let arrive_us = start_us + serial_us;
            queue_free_us = arrive_us;
            let arrival_ms = arrive_us / 1_000 + 15; // + propagation
            batch.push(AckArrival {
                wire_seq: step * 1_000 + i as u32,
                server_emit_us: send_us,
                client_arrival_ms_lo16: (arrival_ms & 0xFFFF) as u16,
                size_bytes: PKT_BYTES,
                probe: None,
            });
        }
        emit_us += 20_000;

        let snap = w.update(&batch, t0 + Duration::from_micros(emit_us));
        send_rate_bps = snap.bitrate_bps.max(200_000);
        out.push(snap.bitrate_bps);
    }
    out
}

#[test]
fn estimate_rises_toward_capacity_on_a_clean_link() {
    let series = run(400, |_| 3_000_000);
    let final_bps = *series.last().unwrap();
    assert!(
        final_bps > 1_000_000,
        "estimate {final_bps} never rose above the 1 Mbit/s seed on a 3 Mbit/s link"
    );
}

#[test]
fn estimate_backs_off_after_a_step_down() {
    // 3 Mbit/s for the first half, 600 kbit/s for the second.
    let series = run(600, |step| if step < 300 { 3_000_000 } else { 600_000 });
    let before = series[299];
    let after = *series.last().unwrap();
    assert!(
        after < before,
        "estimate did not fall after capacity dropped: {before} -> {after}"
    );
}

/// Feed a short, clean-link ACK stream through the real estimator and
/// return the resulting `pacer_rate_bps`. Exercises the actual
/// estimator -> `pacer_config` -> `pacer_rate_bps` pipeline (real feedback,
/// not a hand-picked number), so the budget tests below combine a genuine
/// goog_cc output with a synthetic quinn/AIMD side rather than testing
/// `combine_pacing_budget`'s arithmetic against two synthetic numbers (that
/// coverage already lives in `io_bridge.rs`'s unit tests).
fn real_pacer_rate_bps() -> u64 {
    let t0 = Instant::now();
    let mut w = BweWrapper::new(2_000_000, t0);
    let mut pacer_rate_bps = None;
    for step in 0..50u32 {
        let batch: Vec<AckArrival> = (0..12u32)
            .map(|i| {
                let emit_us = ((step * 20 + i) as u64) * 1_000;
                AckArrival {
                    wire_seq: step * 12 + i,
                    server_emit_us: emit_us,
                    client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                    size_bytes: PKT_BYTES,
                    probe: None,
                }
            })
            .collect();
        let snap = w.update(&batch, t0 + Duration::from_millis(20 * step as u64));
        pacer_rate_bps = snap.pacer_rate_bps;
    }
    pacer_rate_bps.expect("estimator produced no pacer_config after 50 feedback batches")
}

/// The budget must be `min(aimd, googcc)`, not either side alone. This case
/// makes goog_cc the binding constraint: the quinn/path-stats side reports
/// an implausibly generous budget (as if the send buffer were nearly
/// empty), so the combined result must still be bounded by goog_cc's
/// (comparatively tight) pacer rate. A buggy `max` in place of `min` would
/// return the huge AIMD side here and fail this assertion.
#[test]
fn googcc_pacer_binds_when_it_is_the_tighter_constraint() {
    let pacer_rate_bps = real_pacer_rate_bps();
    let googcc_budget = pacer_tick_budget_bytes(pacer_rate_bps, TICK_INTERVAL_US);
    let huge_aimd_budget = googcc_budget * 100 + 1_000_000;

    let combined = combine_pacing_budget(
        PacingMode::Paced,
        huge_aimd_budget,
        Some(pacer_rate_bps),
        TICK_INTERVAL_US,
    );

    assert_eq!(
        combined, googcc_budget,
        "goog_cc's pacer rate should have bound the budget \
         (aimd={huge_aimd_budget}, googcc={googcc_budget}, got={combined})"
    );
    assert!(combined < huge_aimd_budget);
}

/// Same pairing, the other direction: the quinn/AIMD side is now the tight
/// one (as if the send buffer were nearly full), and must bind instead of
/// goog_cc's comparatively generous estimate. Needed alongside the test
/// above — a single-direction test could pass a buggy `max` if that test
/// happened to make the `max`-selected side equal the `min`-selected side,
/// which neither of these two do (each asserts strict inequality against
/// the losing side too).
#[test]
fn quinn_aimd_binds_when_it_is_the_tighter_constraint() {
    let pacer_rate_bps = real_pacer_rate_bps();
    let googcc_budget = pacer_tick_budget_bytes(pacer_rate_bps, TICK_INTERVAL_US);
    let tiny_aimd_budget = (googcc_budget / 100).max(1);

    let combined = combine_pacing_budget(
        PacingMode::Paced,
        tiny_aimd_budget,
        Some(pacer_rate_bps),
        TICK_INTERVAL_US,
    );

    assert_eq!(
        combined, tiny_aimd_budget,
        "quinn/AIMD's budget should have bound \
         (aimd={tiny_aimd_budget}, googcc={googcc_budget}, got={combined})"
    );
    assert!(combined < googcc_budget);
}

/// End-to-end sanity on the mode switch itself: after enough real feedback
/// batches, `samples_seen` clears the (private, `io_bridge`-internal)
/// sample threshold and `select_pacing_mode` reports `Paced` — ties the
/// estimator's own counter to the mode decision rather than only testing
/// `select_pacing_mode` against synthetic sample counts (see `io_bridge.rs`
/// for that coverage).
#[test]
fn pacing_mode_reaches_paced_after_enough_real_feedback() {
    let t0 = Instant::now();
    let mut w = BweWrapper::new(2_000_000, t0);
    let mut samples_seen = 0;
    for step in 0..50u32 {
        let batch: Vec<AckArrival> = (0..12u32)
            .map(|i| {
                let emit_us = ((step * 20 + i) as u64) * 1_000;
                AckArrival {
                    wire_seq: step * 12 + i,
                    server_emit_us: emit_us,
                    client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                    size_bytes: PKT_BYTES,
                    probe: None,
                }
            })
            .collect();
        samples_seen = w
            .update(&batch, t0 + Duration::from_millis(20 * step as u64))
            .samples_seen;
    }
    assert_eq!(
        select_pacing_mode(samples_seen),
        PacingMode::Paced,
        "{samples_seen} ACK samples should be well past the paced-mode threshold"
    );
}

#[test]
fn estimate_survives_a_timestamp_wrap() {
    // The client arrival series is 16-bit ms and wraps every 65_536 ms;
    // 4000 ticks x 20 ms crosses that boundary more than once. Without
    // unwrapping, each wrap poisons the delay gradient.
    let series = run(4_000, |_| 3_000_000);
    let final_bps = *series.last().unwrap();
    assert!(
        final_bps > 1_000_000,
        "estimate {final_bps} collapsed across a timestamp wrap"
    );
}
