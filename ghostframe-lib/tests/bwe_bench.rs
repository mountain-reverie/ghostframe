//! Tier-1 bandwidth-estimator bench.
//!
//! Drives the estimator against a simulated bottleneck: packets serialise at
//! the link rate, so offering more than capacity makes one-way delay grow —
//! the signal a delay-gradient controller is built to detect.
//!
//! Deterministic. Unlike the browserless scenes this is a pure function of
//! its inputs, so any failure here reproduces exactly from the source.

use ghostframe_lib::transport::bwe::{AckArrival, BweWrapper, ProbeRequest};
use ghostframe_lib::transport::io_bridge::{
    combine_pacing_budget, pacer_tick_budget_bytes, select_pacing_mode, PacingMode,
};
use ghostframe_lib::transport::reliable_emitter::cache::ProbeTag;
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

        let snap = w.update(
            &batch,
            t0 + Duration::from_micros(emit_us),
            batch.iter().map(|r| r.size_bytes as usize).sum(),
        );
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
        let snap = w.update(
            &batch,
            t0 + Duration::from_millis(20 * step as u64),
            batch.iter().map(|r| r.size_bytes as usize).sum(),
        );
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
            .update(
                &batch,
                t0 + Duration::from_millis(20 * step as u64),
                batch.iter().map(|r| r.size_bytes as usize).sum(),
            )
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

// ── BWE Stage 2.4: probe clusters — the gate ────────────────────────────────
//
// Three different claims, per the design doc: (1) the config was read, (2)
// packets were tagged, (3) the estimator accepted the cluster. Tasks 1-3
// proved (1) and (2) — a tagged `AckArrival` reaches `PacedPacketInfo`
// intact. Only (3) means probing actually works, and
// `probe_bitrate_estimator.rs:137` discards an under-filled cluster with no
// signal at all. Every test below distinguishes "read" from "consumed".
//
// The cluster requests driving these tests are real, not fabricated:
// `GoogCcNetworkController`'s very first `on_process_interval` call applies
// its `initial_config` via `reset_constraints`, which calls
// `ProbeController::set_bitrates` while the controller is still in its
// startup `State::Init` — that unconditionally fires the exponential-probing
// path (`initiate_exponential_probing`: one cluster at 3x the seed rate, one
// at 6x), and `absorb` (`googcc.rs`) keeps the more recent (6x) one as
// `pending_probe_request`. A single ACK-arrival batch is enough to trigger
// it. (This only fires at all because `GoogCcDriver::new` now calls
// `on_network_availability` — see that constructor's doc comment: without
// it `ProbeController` is stuck in `State::Init` forever and never proposes
// a cluster, which is what this bench found before that fix landed.)

/// Trigger goog_cc's real initial-probing path and return the wrapper, its
/// clock base, and the `ProbeRequest` it surfaced. Panics if none arrives —
/// that would mean `ProbeController` never left `State::Init`, which is
/// itself the finding to report, not something to paper over with a
/// fabricated `ProbeRequest`.
fn request_a_real_probe(seed_bps: u64) -> (BweWrapper, Instant, ProbeRequest) {
    let t0 = Instant::now();
    let mut w = BweWrapper::new(seed_bps, t0);
    // Any non-empty batch trips `on_process_interval`; the content doesn't
    // matter to the probe path, only that `update()` runs once.
    let seed_batch = vec![AckArrival {
        wire_seq: 0,
        server_emit_us: 0,
        client_arrival_ms_lo16: 15,
        size_bytes: PKT_BYTES,
        probe: None,
    }];
    w.update(
        &seed_batch,
        t0 + Duration::from_millis(20),
        seed_batch.iter().map(|r| r.size_bytes as usize).sum(),
    );
    let req = w.take_probe_request().unwrap_or_else(|| {
        panic!(
            "goog_cc's ProbeController did not request an initial probe \
             cluster from a single seed batch — the exponential-probing \
             path did not fire (see GoogCcDriver::new's on_network_availability \
             call, required for ProbeController to ever leave State::Init)"
        )
    });
    (w, t0, req)
}

/// Build one tagged `AckArrival`, the `i`-th of `n` packets in a fill
/// attempt against `req`, each `pkt_bytes` and 1 ms apart. `bytes_sent`
/// (the cluster's running total *before* this packet) is threaded through
/// by the caller, mirroring `ProbeTag::bytes_sent_before`'s real semantics.
fn tagged_probe_packet(
    req: &ProbeRequest,
    i: u32,
    pkt_bytes: u32,
    bytes_sent_before: i64,
) -> AckArrival {
    let send_us = 100_000 + (i as u64) * 1_000;
    let arrival_ms = send_us / 1_000 + 10 + i as u64;
    AckArrival {
        wire_seq: 1_000 + i,
        server_emit_us: send_us,
        client_arrival_ms_lo16: (arrival_ms & 0xFFFF) as u16,
        size_bytes: pkt_bytes,
        probe: Some(ProbeTag {
            id: req.id,
            min_probes: req.min_probes,
            min_bytes: req.min_bytes,
            bytes_sent_before,
        }),
    }
}

/// Step 1 — a cluster is emitted *and consumed*. Fill a real, requested
/// cluster comfortably past both thresholds and assert the estimate moves.
///
/// The control (identical timing and sizes, `probe: None`) is the point:
/// any feedback batch nudges the acknowledged-bitrate estimator a little,
/// so "the number changed" alone would not prove the *probe* path fired.
/// The tagged run must move dramatically more than that baseline —
/// specifically, toward the cluster's target rate — which only the probe
/// estimator's short-circuit into `set_send_bitrate` can produce from an
/// 11-packet batch.
#[test]
fn a_requested_probe_cluster_is_filled_and_consumed() {
    let (mut w, t0, req) = request_a_real_probe(1_000_000);
    let before = w.snapshot().bitrate_bps;

    // Comfortably past both min_probes and min_bytes -- the estimator's own
    // margin is 80% of each (probe_bitrate_estimator.rs's
    // MIN_RECEIVED_PROBES_RATIO / MIN_RECEIVED_BYTES_RATIO), so this uses a
    // healthy multiple of the raw thresholds rather than the exact floor.
    let n = (req.min_probes as u32 * 2).max(8);
    let pkt_bytes = ((req.min_bytes as u64 / n as u64) + 100) as u32;
    assert!(
        (n as i64) >= req.min_probes && (n as i64 * pkt_bytes as i64) >= req.min_bytes,
        "test construction bug: fill batch (n={n}, bytes={pkt_bytes}) does not \
         actually clear min_probes={} / min_bytes={}",
        req.min_probes,
        req.min_bytes
    );

    let mut bytes_sent_before = 0i64;
    let batch: Vec<AckArrival> = (0..n)
        .map(|i| {
            let pkt = tagged_probe_packet(&req, i, pkt_bytes, bytes_sent_before);
            bytes_sent_before += pkt_bytes as i64;
            pkt
        })
        .collect();
    let after = w
        .update(
            &batch,
            t0 + Duration::from_millis(200),
            batch.iter().map(|r| r.size_bytes as usize).sum(),
        )
        .bitrate_bps;

    // Control: the same shape of traffic, untagged, from a fresh wrapper
    // seeded identically -- isolates "any feedback moves the number a
    // little" from "the probe estimator accepted this cluster".
    let (mut control, ct0, _) = request_a_real_probe(1_000_000);
    let control_before = control.snapshot().bitrate_bps;
    let control_batch: Vec<AckArrival> = (0..n)
        .map(|i| AckArrival {
            probe: None,
            ..tagged_probe_packet(&req, i, pkt_bytes, 0)
        })
        .collect();
    let control_after = control
        .update(
            &control_batch,
            ct0 + Duration::from_millis(200),
            control_batch.iter().map(|r| r.size_bytes as usize).sum(),
        )
        .bitrate_bps;

    assert_ne!(
        after, before,
        "estimate did not move at all after a filled, requested probe cluster"
    );
    assert!(
        after > control_after * 2,
        "tagged fill (before={before} after={after}) did not move \
         substantially more than an identically-shaped untagged control \
         (before={control_before} after={control_after}) -- this is the \
         evidence that the *probe* path fired, not just routine feedback \
         absorption"
    );
}

/// Step 2 — an under-filled cluster is discarded, silently, exactly as
/// `probe_bitrate_estimator.rs:137` documents. Same real request as step 1,
/// but the fill stays under the estimator's byte threshold (well under even
/// its 80%-of-min_bytes acceptance margin) while still meeting
/// `min_probes` on packet count alone.
///
/// Without this test, step 1 could pass while every real probe silently
/// fails: an over-filled cluster trivially satisfies both thresholds, and
/// nothing else distinguishes "the estimator is wired correctly" from "the
/// estimator ignores the tag and something else moved the number".
#[test]
fn an_underfilled_probe_cluster_is_discarded_silently() {
    let (mut w, t0, req) = request_a_real_probe(1_000_000);
    let before = w.snapshot().bitrate_bps;

    // Meets min_probes on count, but well under 80% of min_bytes.
    let n = (req.min_probes as u32).max(5);
    let pkt_bytes = ((req.min_bytes as u64 / n as u64) / 4).max(1) as u32;
    let total_bytes = n as i64 * pkt_bytes as i64;
    assert!(
        total_bytes < (req.min_bytes as f64 * 0.8) as i64,
        "test construction bug: fill batch (total={total_bytes}) is not \
         actually under the estimator's 80%-of-min_bytes threshold ({})",
        (req.min_bytes as f64 * 0.8) as i64
    );

    let mut bytes_sent_before = 0i64;
    let batch: Vec<AckArrival> = (0..n)
        .map(|i| {
            let pkt = tagged_probe_packet(&req, i, pkt_bytes, bytes_sent_before);
            bytes_sent_before += pkt_bytes as i64;
            pkt
        })
        .collect();
    let after = w
        .update(
            &batch,
            t0 + Duration::from_millis(200),
            batch.iter().map(|r| r.size_bytes as usize).sum(),
        )
        .bitrate_bps;

    // Control: an identically-shaped, fully untagged batch from a fresh,
    // identically-seeded wrapper. The under-filled cluster's effect on the
    // estimate should be indistinguishable from ordinary traffic of the
    // same size and timing -- that is what "silently discarded" means.
    let (mut control, ct0, _) = request_a_real_probe(1_000_000);
    let control_batch: Vec<AckArrival> = (0..n)
        .map(|i| AckArrival {
            probe: None,
            ..tagged_probe_packet(&req, i, pkt_bytes, 0)
        })
        .collect();
    let control_after = control
        .update(
            &control_batch,
            ct0 + Duration::from_millis(200),
            control_batch.iter().map(|r| r.size_bytes as usize).sum(),
        )
        .bitrate_bps;

    assert_eq!(
        after, control_after,
        "before={before} after={after} (control={control_after}) -- an \
         under-filled cluster should land exactly like ordinary untagged \
         traffic of the same shape, not partially move the estimate"
    );
}

/// Step 3 — the `min_bytes` derivation is right. goog_cc offers no helper
/// for `target_data_rate x target_duration` (`PacedPacketInfo::new` takes
/// `min_bytes` as a plain `i64`), so `to_probe_request` (`googcc.rs`)
/// computes it itself. A silent error there makes every cluster either
/// trivially pass (derived value too low) or never pass (too high), and
/// this checks it against a real, controller-produced `ProbeRequest`
/// rather than a hand-picked one.
#[test]
fn probe_request_min_bytes_is_target_rate_times_duration() {
    let (_w, _t0, req) = request_a_real_probe(1_000_000);

    // bits/sec * microseconds / 8 (bits->bytes) / 1_000_000 (us->s) = bytes,
    // mirroring `GoogCcDriver::to_probe_request`'s own derivation exactly.
    let expected_min_bytes =
        (req.target_rate_bps as i64 * req.duration.as_micros() as i64) / 8_000_000;

    assert_eq!(
        req.min_bytes, expected_min_bytes,
        "min_bytes ({}) does not equal target_rate_bps ({}) x duration ({:?}) \
         -- the derivation in GoogCcDriver::to_probe_request has drifted",
        req.min_bytes, req.target_rate_bps, req.duration
    );
    // Sanity: goog_cc's exponential probing asks for a *ladder* from a
    // 1 Mbit/s seed -- 3x then 6x -- at 5 minimum packets. Pin the concrete
    // numbers so a change in goog_cc's own defaults (a dependency bump) is
    // visible here rather than only in the formula check.
    //
    // The first request surfaced is the *lower* rung. Until 2026-09-14 the
    // driver kept only the last config of each batch and this assertion read
    // 6_000_000: the 3 Mbit/s rung was being discarded, which is the one more
    // likely to be answerable on a slow link.
    assert_eq!(req.target_rate_bps, 3_000_000);
    assert_eq!(req.min_probes, 5);

    // goog_cc asks for 15 ms, sized for a continuous pacer. `to_probe_request`
    // floors it at 3 scheduler ticks because our emission is frame-quantised:
    // every datagram in one tick carries the same `emit_us`, so a window
    // narrower than a tick can only ever hold one burst and goog_cc rejects
    // the cluster outright with `send interval: 0 us`. Both the widened
    // duration and the `min_bytes` derived from it are pinned here, since a
    // duration floor that failed to scale `min_bytes` would make every
    // cluster trivially completable.
    assert!(
        req.duration >= Duration::from_micros(99_999),
        "probe window must span multiple scheduler ticks, got {:?}",
        req.duration
    );
    assert_eq!(req.min_bytes, 37_499);
}

/// goog_cc asks for a ladder of probe clusters, not one; every rung must
/// survive to the caller.
///
/// The driver used to keep only the last config of each `NetworkControlUpdate`,
/// reasoning that a config never surfaced is indistinguishable from one never
/// requested. That holds for any single config and fails for the set: the
/// first request of a session is a pair, and discarding the lower rung throws
/// away the probe more likely to be answerable on a slow link. Measured on a
/// browserless scene, the discarded rung was 6 Mbps of a [6, 12] Mbps pair
/// against a 480 kbps link.
#[test]
fn every_rung_of_a_probe_ladder_is_surfaced_in_order() {
    let (mut w, _t0, first) = request_a_real_probe(1_000_000);

    let second = w
        .take_probe_request()
        .expect("the second rung of the ladder must still be pending");
    assert!(
        second.target_rate_bps > first.target_rate_bps,
        "rungs must surface lowest-first so the answerable one is tried \
         first: got {} then {}",
        first.target_rate_bps,
        second.target_rate_bps
    );
    assert_eq!(
        (first.target_rate_bps, second.target_rate_bps),
        (3_000_000, 6_000_000),
        "goog_cc's exponential ladder from a 1 Mbit/s seed is 3x then 6x"
    );
    assert!(
        w.take_probe_request().is_none(),
        "the ladder had two rungs; a third would mean requests are accumulating"
    );
}

/// Step 4 — untagged traffic is unaffected. `ProbeController` requests a
/// cluster from the very first batch (see `request_a_real_probe`), but
/// ordinary traffic that never carries the tag must behave exactly as it
/// did before probing existed: a clean link's estimate still rises above
/// its seed. This is the same assertion as `wrapper_rises_on_a_clean_link`
/// in `bwe/mod.rs`, run here specifically *with* a pending, unconsumed
/// probe request in play, to prove the two paths don't interact.
#[test]
fn untagged_traffic_tracks_normally_around_a_pending_probe() {
    let (mut w, t0, _req) = request_a_real_probe(4_000_000);

    let mut last = w.snapshot().bitrate_bps;
    for step in 0..400u32 {
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
        last = w
            .update(
                &batch,
                t0 + Duration::from_millis(20 * step as u64),
                batch.iter().map(|r| r.size_bytes as usize).sum(),
            )
            .bitrate_bps;
    }

    assert!(
        last > 4_000_000,
        "estimate {last} did not rise on a clean link with an unconsumed \
         probe request pending -- untagged traffic should be unaffected by \
         probe machinery it never opted into"
    );
}
