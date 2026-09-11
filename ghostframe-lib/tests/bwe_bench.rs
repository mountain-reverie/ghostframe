//! Tier-1 bandwidth-estimator bench.
//!
//! Drives the estimator against a simulated bottleneck: packets serialise at
//! the link rate, so offering more than capacity makes one-way delay grow —
//! the signal a delay-gradient controller is built to detect.
//!
//! Deterministic. Unlike the browserless scenes this is a pure function of
//! its inputs, so any failure here reproduces exactly from the source.

use ghostframe_lib::transport::bwe::{AckArrival, BweWrapper};
use std::time::{Duration, Instant};

const PKT_BYTES: u32 = 1200;

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
