//! The two capacity-estimation scenes, in their own binary.
//!
//! They live here rather than in `browserless_runner` because of how they

use ghostframe_e2e::harness::browserless::{run_browserless, BrowserlessScene, SceneLoad};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, NetProfile};
use std::time::Duration;

/// Run a sustained production-cadence scene over a queueing bottleneck of a
/// given capacity, and report what the estimator made of it.
async fn run_bottleneck_scene(
    seed: u64,
    cap: CapTimeline,
    secs: u64,
) -> ghostframe_e2e::harness::browserless::BrowserlessResult {
    let scene = BrowserlessScene {
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
            cap,
            bottleneck: Some(Bottleneck::wifi()),
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(secs),
        grid_cols: 16,
        grid_rows: 16,
    };
    run_browserless(scene).await.expect("scene ran")
}

/// The estimator must tell a congested link from an uncongested one.
///
/// This deliberately does **not** assert convergence to the link rate, which
/// would be false: on the congested link the estimate sits on goog_cc's
/// `MIN_BPS` floor (200 kbps), and on the uncongested one it stays near its
/// 2 Mbps seed because nothing ever signals congestion. Asserting "within a
/// factor of capacity" would fail on both sides for opposite reasons.
///
/// What it does assert is the property that makes an estimator an estimator:
/// a link that queues and drops must produce a materially lower estimate than
/// one with headroom to spare. Measured separation is ~10x with no run-to-run
/// variance, so the 2x threshold here has a wide margin.
///
/// On the bottleneck's role, stated precisely, because an induced-failure
/// check refuted the stronger claim this comment first made: the *estimate
/// separation* above is visible against the old drop-without-queueing cap
/// too (201 kbps vs 2.02 Mbps measured). What the bottleneck changes is the
/// `bytes_dropped` guards. A token bucket drops bursts even on a link with
/// ample headroom — 1,200 bytes shed from the spacious scene — which is not
/// how an uncongested link behaves, so `dropped == 0` is assertable only
/// against a queue. It also makes the scene reproducible: queue occupancy is
/// a deterministic function of arrivals, where the bucket's drop decisions
/// were timing-sensitive and gave estimates spanning 215 kbps to 2.0 Mbps on
/// identical inputs.
///
/// The queueing model still matters for the wider point — without a
/// queuing-delay gradient goog_cc's delay-based half never runs at all — but
/// that is not what *this* assertion rests on. See
/// `docs/specs/bwe-probe-emission-timing.md`.
#[tokio::test(start_paused = true)]
async fn the_estimate_separates_a_congested_link_from_an_uncongested_one() {
    // Offered load is ~1.6 Mbps, so 480 kbps congests and 3.2 Mbps does not.
    let congested = run_bottleneck_scene(0xC0FF_EE01, CapTimeline::constant(500_000), 20).await;
    let spacious = run_bottleneck_scene(0xC0FF_EE01, CapTimeline::constant(2_000_000), 20).await;

    println!(
        "congested: est={} pacer={:?} s2c={} c2s={} dropped={} retx={}\n\
         spacious:  est={} pacer={:?} s2c={} c2s={} dropped={} retx={}",
        congested.bwe_estimate_bps,
        congested.pacer_rate_bps,
        congested.bytes_delivered_s2c,
        congested.bytes_delivered_c2s,
        congested.bytes_dropped,
        congested.retransmit_attempts_total,
        spacious.bwe_estimate_bps,
        spacious.pacer_rate_bps,
        spacious.bytes_delivered_s2c,
        spacious.bytes_delivered_c2s,
        spacious.bytes_dropped,
        spacious.retransmit_attempts_total,
    );

    // The scenes must actually be what they claim, or the comparison below
    // is between two identical links and proves nothing.
    assert!(
        congested.bytes_dropped > 0,
        "the congested scene must overflow its buffer; dropped={}",
        congested.bytes_dropped
    );
    assert_eq!(
        spacious.bytes_dropped, 0,
        "the spacious scene must have headroom to spare, but dropped {}",
        spacious.bytes_dropped
    );

    assert!(
        spacious.bwe_estimate_bps > congested.bwe_estimate_bps * 2,
        "a link with headroom must estimate materially higher than a congested \
         one: spacious={} congested={}",
        spacious.bwe_estimate_bps,
        congested.bwe_estimate_bps
    );
}

/// Capacity triples mid-session: does the estimate find the new headroom?
///
/// This is the scenario probing exists for, and the one no test covered.
/// A session that starts congested and is then handed room to grow has to
/// *discover* that room — nothing tells it. goog_cc's answer is to probe:
/// send a short burst above the current estimate and read the ACKs.
///
/// Probe clusters complete here, which is itself new: at production cadence
/// over the old drop-without-queueing cap they never did. So probing works
/// and any slow ramp is not explained by probe abandonment — which bounds
/// how much `drain_for_probe_window_open` could be worth, the open question
/// in `docs/specs/bwe-probe-emission-timing.md`.
///
/// # Why the thresholds are what they are
///
/// This test was ~50% flaky until 2026-09-20, and re-measuring showed the
/// cause was not noise but two threshold choices that did not match the
/// behaviour being measured.
///
/// **The acceptance level.** Measured final estimates on this 16 Mbps
/// post-step link, 8 runs each:
///
/// | | final estimate |
/// |---|---|
/// | with `set_transport_capacity_hint` | 12.0 - 18.2 Mbps |
/// | with the hint disabled | 7.0 - 7.2 Mbps |
///
/// The old 80% bar (12.8 Mbps) cut straight through the *with-hint* range,
/// so the test was a coin flip on its own success case. 60% (9.6 Mbps) sits
/// in the gap between the two populations: 34% above the no-hint maximum and
/// 20% below the with-hint minimum. It is a weaker-sounding number that
/// discriminates strictly better, and it is still far more than the
/// "doubling" an earlier version asserted — a 2 -> 4.1 Mbps move would not
/// come close.
///
/// **The observation window.** The scene ran 20 s with the step at 4 s, so it
/// could only ever watch 16 s of recovery — against an 18 s bound. Part of
/// the acceptance range was unobservable by construction. The scene is now
/// 26 s, giving 22 s of post-step observation against the same 18 s bound.
///
/// Time-to-converge at the 60% level now measures 4.4 - 10.1 s across 8 runs,
/// so 18 s carries real margin. Note the bound is no longer what guards
/// against the hint regressing: with the hint disabled the estimate does not
/// reach 60% *at all* within the scene, in any run. Convergence happening is
/// the discriminator; the bound just keeps "eventually" honest.
#[tokio::test(start_paused = true)]
async fn the_estimate_follows_a_mid_scene_capacity_step_up() {
    const LOW: u64 = 500_000; // bytes/s -> 4 Mbps, well below the ~10 Mbps offered
    const HIGH: u64 = 2_000_000; // bytes/s -> 16 Mbps, ample headroom
    const STEP_AT_US: u64 = 4_000_000;
    /// The acceptance bound: the estimate must reach `CONVERGED_FRACTION` of
    /// the new capacity within this long after the step.
    ///
    /// Measured 4.4-10.1 s across 8 runs at the 60% level, so this carries
    /// roughly 80% margin over the observed worst case. It must also stay
    /// under the post-step observation window (26 s scene - 4 s step = 22 s),
    /// or part of the range it admits can never be observed — which is
    /// exactly what made the old 18 s bound unreachable against a 16 s
    /// window.
    ///
    /// Before `set_transport_capacity_hint` fed goog_cc an independent
    /// ceiling, the estimate could only climb by `AimdRateControl`'s
    /// multiplicative increase, which hardcodes `alpha = 1.08` capped to one
    /// second of effect (8% per second). ALR probing cannot substitute for
    /// it: `time_for_alr_probe` fires only when the application is
    /// under-sending, which is exactly when there is too little traffic to
    /// fill a probe cluster. Disabling the hint and re-running confirms it:
    /// the estimate plateaus at ~7.1 Mbps and never reaches this bar.
    const CONVERGE_BY_US: u64 = 18_000_000;
    /// See the "Why the thresholds are what they are" section above: this
    /// sits in the measured gap between the with-hint and no-hint
    /// populations, where 0.8 sat inside the with-hint spread.
    const CONVERGED_FRACTION: f64 = 0.6;

    let r = run_bottleneck_scene(
        0xC0FF_EE02,
        CapTimeline::step(LOW, STEP_AT_US, HIGH),
        // 26 s, not 20: the step lands at 4 s, so this is what makes the
        // post-step observation window (22 s) longer than CONVERGE_BY_US.
        std::env::var("GF_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(26),
    )
    .await;

    let median = |mut v: Vec<u64>| -> u64 {
        v.sort_unstable();
        v[v.len() / 2]
    };

    // Timestamped series, so a window artifact cannot be mistaken for
    // estimator behaviour. An earlier version of this test used a fixed
    // [4s, 5s) window for the pre-step value and was flaky for exactly that
    // reason: how long the estimate stays at its 2 Mbps seed before
    // congestion is detected varies run to run, so the window sometimes
    // averaged the seed instead of the converged floor.
    let series: Vec<String> = r
        .bwe_estimate_samples
        .iter()
        .map(|(t, b)| format!("{}ms:{}", t / 1000, b))
        .collect();
    println!("step-up series: {}", series.join(" "));

    // Pre-step level, measured from 1 s in so the 2 Mbps seed has had time
    // to be replaced by something the link actually justifies. This used to
    // assert the estimate was pinned at `MIN_BPS` exactly, which only held
    // because the old scene offered so little that goog_cc was driven into
    // its floor and parked there. A scene that genuinely saturates its link
    // settles on a real value instead (~1.3 Mbps on the 4 Mbps cap), so
    // "did it climb" is both the honest question and the one with margin.
    let pre_step: Vec<u64> = r
        .bwe_estimate_samples
        .iter()
        .filter(|(t, _)| *t >= 1_000_000 && *t < STEP_AT_US)
        .map(|(_, b)| *b)
        .collect();
    // The last three seconds: goog_cc does not react instantly and the
    // question is whether it gets there at all, not how fast.
    let tail: Vec<u64> = r
        .bwe_estimate_samples
        .iter()
        .filter(|(t, _)| *t >= r.bwe_estimate_samples.last().unwrap().0 - 3_000_000)
        .map(|(_, b)| *b)
        .collect();

    println!(
        "step-up: converged_at={:?}ms pre_step(median)={} tail(median)={} probes={}/{} pacer={:?}",
        r.bwe_estimate_samples
            .iter()
            .find(|(t, b)| *t >= STEP_AT_US
                && (*b as f64) >= (HIGH * 8) as f64 * CONVERGED_FRACTION)
            .map(|(t, _)| (t - STEP_AT_US) / 1000),
        median(pre_step.clone()),
        median(tail.clone()),
        r.probes_completed,
        r.probes_abandoned,
        r.pacer_rate_bps,
    );

    assert!(
        !pre_step.is_empty(),
        "need estimate samples between 1 s and the step at {STEP_AT_US} us"
    );
    // The pre-step link carries 4 Mbps against ~10 Mbps offered, so the
    // estimate must sit at or below that — an estimate anywhere near the
    // post-step capacity would mean the scene never congested and there is
    // no headroom discovery to observe.
    //
    // Bounded by the *actual* pre-step capacity rather than a hand-picked
    // number: an estimate above the link's real rate is an overestimate,
    // which is a bug in its own right and the dangerous direction. An
    // earlier version used a flat 3 Mbps and started failing when the
    // estimator got *better* (1.9 -> 3.03 Mbps on a 4 Mbps link), which is
    // the wrong thing for a test to punish.
    assert!(
        median(pre_step.clone()) <= LOW * 8,
        "the pre-step link carries {} bits/s; an estimate above that is an \
         overestimate, not congestion: pre_step(median)={}",
        LOW * 8,
        median(pre_step.clone())
    );
    assert!(
        !tail.is_empty(),
        "need estimate samples in the final seconds of the scene"
    );
    // The acceptance criterion proper: not merely "it moved", but that it
    // reached the new capacity, and did so within a stated bound. An earlier
    // version asserted only a doubling, which a 2 -> 4.1 Mbps move on a
    // 16 Mbps link would have satisfied while missing the cap four-fold.
    let target_bps = (HIGH * 8) as f64 * CONVERGED_FRACTION;
    let converged_at = r
        .bwe_estimate_samples
        .iter()
        .find(|(t, b)| *t >= STEP_AT_US && (*b as f64) >= target_bps)
        .map(|(t, _)| t - STEP_AT_US);

    match converged_at {
        Some(dt) => assert!(
            dt <= CONVERGE_BY_US,
            "capacity went {} -> {} bits/s at {STEP_AT_US}us; the estimate reached \
             {:.0}% of it only after {}ms, past the {}ms bound. pre_step(median)={} \
             tail(median)={}",
            LOW * 8,
            HIGH * 8,
            CONVERGED_FRACTION * 100.0,
            dt / 1000,
            CONVERGE_BY_US / 1000,
            median(pre_step.clone()),
            median(tail.clone())
        ),
        None => panic!(
            "capacity went {} -> {} bits/s at {STEP_AT_US}us and the estimate never \
             reached {:.0}% of it before the scene ended. pre_step(median)={} \
             tail(median)={}. Never discovering the new headroom is a finding, \
             not a flaky test",
            LOW * 8,
            HIGH * 8,
            CONVERGED_FRACTION * 100.0,
            median(pre_step.clone()),
            median(tail.clone())
        ),
    }
}
