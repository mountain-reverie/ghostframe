//! Tests for the netsim module: RNG determinism and loss rate accuracy.

use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, DetRng, NetProfile, NetSim, Verdict};

#[test]
fn identical_seeds_produce_identical_streams() {
    let mut a = DetRng::new(0xDEAD_BEEF);
    let mut b = DetRng::new(0xDEAD_BEEF);
    let xs: Vec<u64> = (0..64).map(|_| a.next_u64()).collect();
    let ys: Vec<u64> = (0..64).map(|_| b.next_u64()).collect();
    assert_eq!(xs, ys);

    let mut c = DetRng::new(0xDEAD_BEEE);
    let zs: Vec<u64> = (0..64).map(|_| c.next_u64()).collect();
    assert_ne!(xs, zs, "different seeds must diverge");
}

#[test]
fn measured_loss_rate_matches_the_configuration() {
    let profile = NetProfile {
        loss: 0.10,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 42);
    let n = 100_000;
    let dropped = (0..n)
        .filter(|_| matches!(sim.decide(64, 0), Verdict::Drop))
        .count();
    let rate = dropped as f64 / n as f64;
    assert!(
        (rate - 0.10).abs() < 0.005,
        "measured loss {rate:.4} must be within 0.5% of 0.10"
    );
}

#[test]
fn burst_loss_clusters_drops() {
    let profile = NetProfile {
        burst_enter: 0.01,
        burst_exit: 0.20,
        burst_loss: 0.90,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 7);
    let drops: Vec<bool> = (0..20_000)
        .map(|_| matches!(sim.decide(64, 0), Verdict::Drop))
        .collect();

    // A clustered process has a much higher P(drop | previous dropped) than
    // its unconditional drop rate.
    let total = drops.iter().filter(|d| **d).count() as f64;
    let pairs = drops.windows(2).filter(|w| w[0] && w[1]).count() as f64;
    let p_uncond = total / drops.len() as f64;
    let p_cond = pairs / total;
    assert!(
        p_cond > p_uncond * 3.0,
        "burst loss must cluster: P(drop|drop)={p_cond:.3} vs P(drop)={p_uncond:.3}"
    );
}

#[test]
fn delay_and_jitter_stay_within_bounds() {
    let profile = NetProfile {
        delay_us: 20_000,
        jitter_us: 5_000,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 3);
    for _ in 0..1_000 {
        match sim.decide(64, 100_000) {
            Verdict::Deliver { at_us } => {
                assert!((115_000..=125_000).contains(&at_us), "at_us={at_us}");
            }
            other => panic!("expected Deliver, got {other:?}"),
        }
    }
}

#[test]
fn duplication_fires_at_roughly_the_configured_rate_and_dup_at_us_is_ordered() {
    let profile = NetProfile {
        duplicate: 0.20,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 11);
    let n = 50_000;
    let mut dup_count = 0usize;
    for _ in 0..n {
        match sim.decide(64, 1_000) {
            Verdict::Duplicate { at_us, dup_at_us } => {
                dup_count += 1;
                assert!(
                    dup_at_us >= at_us,
                    "dup_at_us ({dup_at_us}) must be >= at_us ({at_us})"
                );
            }
            Verdict::Deliver { .. } => {}
            other => panic!("expected Deliver or Duplicate, got {other:?}"),
        }
    }
    let rate = dup_count as f64 / n as f64;
    assert!(
        (rate - 0.20).abs() < 0.01,
        "measured duplication rate {rate:.4} must be within 1% of 0.20"
    );
}

#[test]
fn corruption_fires_at_roughly_the_configured_rate_and_bit_index_in_range() {
    let profile = NetProfile {
        corrupt: 0.20,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 13);
    let n = 50_000;
    let len = 64usize;
    let mut corrupt_count = 0usize;
    for _ in 0..n {
        match sim.decide(len, 1_000) {
            Verdict::Corrupt { bit_index, .. } => {
                corrupt_count += 1;
                assert!(
                    bit_index < len * 8,
                    "bit_index ({bit_index}) must be < len*8 ({})",
                    len * 8
                );
            }
            Verdict::Deliver { .. } => {}
            other => panic!("expected Deliver or Corrupt, got {other:?}"),
        }
    }
    let rate = corrupt_count as f64 / n as f64;
    assert!(
        (rate - 0.20).abs() < 0.01,
        "measured corruption rate {rate:.4} must be within 1% of 0.20"
    );
}

#[test]
fn fixed_seed_full_profile_is_bit_for_bit_reproducible() {
    // Regression test protecting every recorded seed in every future failure
    // report. It makes two assertions with distinct jobs, and both are needed:
    //
    //  - run() == run() catches non-determinism given identical inputs, but
    //    cannot catch a changed draw order (`decide` is a pure function of
    //    seed, profile and inputs, so two runs in one build always agree).
    //  - the GOLDEN_DIGEST comparison below is the one that catches a changed
    //    rng algorithm or draw order, by pinning the values this profile
    //    produced when the constant was recorded.
    fn run() -> Vec<Verdict> {
        let profile = NetProfile {
            loss: 0.05,
            burst_enter: 0.02,
            burst_exit: 0.30,
            burst_loss: 0.50,
            duplicate: 0.05,
            corrupt: 0.05,
            delay_us: 10_000,
            jitter_us: 2_000,
            reorder_us: 500,
            ..NetProfile::perfect()
        };
        let mut sim = NetSim::new(profile, 1234);
        (0..500)
            .map(|i| sim.decide(64 + (i % 200), 1_000 * i as u64))
            .collect()
    }

    let a = run();
    let b = run();
    assert_eq!(
        a, b,
        "identical seed + inputs must produce identical Verdicts"
    );

    // ...and the same values it produced when this constant was recorded.
    //
    // The comparison above cannot fail: `decide` is a pure function of
    // (seed, profile, inputs), so two runs in one build always agree even if
    // the algorithm changed. Only a golden catches the failure that actually
    // matters — swapping SplitMix64 for something else, or reordering the rng
    // draws — either of which silently invalidates every seed recorded in
    // every past failure report while every other test stays green.
    //
    // If this fails, do not re-record the constant to make it pass unless the
    // change to the generator or the draw order was deliberate. Re-recording
    // is declaring every previously-reported seed unreproducible.
    assert_eq!(
        digest(&a),
        GOLDEN_DIGEST,
        "netsim verdict stream changed for a fixed seed — the rng algorithm \
         or the draw order moved, which invalidates recorded seeds"
    );
}

/// Recorded from the sequence above. See the note in the test before changing.
const GOLDEN_DIGEST: u64 = 7_099_419_336_748_855_738;

/// Order-sensitive fold over the verdict stream. Deliberately hand-rolled and
/// stable: a `DefaultHasher` is explicitly not guaranteed stable across Rust
/// releases, which would turn this golden into a spurious failure on a
/// toolchain bump.
fn digest(verdicts: &[Verdict]) -> u64 {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        acc ^= v;
        acc = acc.wrapping_mul(0x1000_0000_01b3);
    };
    for v in verdicts {
        match v {
            Verdict::Drop => mix(1),
            Verdict::Deliver { at_us } => {
                mix(2);
                mix(*at_us);
            }
            Verdict::Duplicate { at_us, dup_at_us } => {
                mix(3);
                mix(*at_us);
                mix(*dup_at_us);
            }
            Verdict::Corrupt { at_us, bit_index } => {
                mix(4);
                mix(*at_us);
                mix(*bit_index as u64);
            }
        }
    }
    acc
}

#[test]
fn delivered_rate_tracks_the_cap_across_a_step_down() {
    let profile = NetProfile {
        cap: CapTimeline::step(1_000_000, 500_000, 250_000), // 1 MB/s, then 250 kB/s at t=0.5s
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 11);

    // Offer 1200-byte datagrams every 500 µs for one second.
    let mut delivered_before = 0usize;
    let mut delivered_after = 0usize;
    let mut now_us = 0u64;
    while now_us < 1_000_000 {
        if !matches!(sim.decide(1200, now_us), Verdict::Drop) {
            if now_us < 500_000 {
                delivered_before += 1200;
            } else {
                delivered_after += 1200;
            }
        }
        now_us += 500;
    }

    let bps_before = delivered_before as f64 * 2.0; // half a second
    let bps_after = delivered_after as f64 * 2.0;
    assert!(
        (bps_before - 1_000_000.0).abs() < 150_000.0,
        "pre-step rate {bps_before} must track 1 MB/s"
    );
    assert!(
        (bps_after - 250_000.0).abs() < 50_000.0,
        "post-step rate {bps_after} must track 250 kB/s"
    );
}

#[test]
fn adding_a_cap_does_not_shift_the_rng_draw_sequence() {
    // The cap must be applied *after* every rng draw, never as an early
    // return. If a bandwidth drop short-circuited ahead of the draws, a
    // profile merely having a cap would consume a different number of rng
    // values and desynchronise the stream — silently invalidating every
    // seed recorded in a past failure report.
    //
    // The golden test cannot catch that: its profile is uncapped, so the
    // bucket never bites there. This one runs the same seed and the same
    // impairment profile twice, differing *only* in the cap, and asserts the
    // surviving verdicts are positionally identical to the uncapped run.
    // Under a correct implementation the cap can subtract deliveries but can
    // never alter one, because it draws nothing.
    fn run(cap: CapTimeline) -> Vec<Verdict> {
        let profile = NetProfile {
            loss: 0.05,
            burst_enter: 0.02,
            burst_exit: 0.30,
            burst_loss: 0.50,
            duplicate: 0.05,
            corrupt: 0.05,
            delay_us: 10_000,
            jitter_us: 2_000,
            reorder_us: 500,
            cap,
            bottleneck: None,
        };
        let mut sim = NetSim::new(profile, 1234);
        (0..500).map(|i| sim.decide(1200, 500 * i as u64)).collect()
    }

    let uncapped = run(CapTimeline::unlimited());
    let capped = run(CapTimeline::constant(400_000));

    let mut converted_to_drop = 0;
    for (i, (u, c)) in uncapped.iter().zip(capped.iter()).enumerate() {
        if u == c {
            continue;
        }
        match (u, c) {
            // The cap may drop a datagram the uncapped link delivered...
            (_, Verdict::Drop) => converted_to_drop += 1,
            // ...or afford one copy of a duplicate but not both, keeping the
            // original's arrival time untouched.
            (Verdict::Duplicate { at_us: uat, .. }, Verdict::Deliver { at_us: cat })
                if uat == cat => {}
            _ => panic!(
                "verdict {i} changed in a way the cap cannot explain: \
                 uncapped {u:?} vs capped {c:?} — the draw order moved"
            ),
        }
    }

    // Guard against the assertion above passing vacuously: if the cap never
    // bit, the two runs would be identical and prove nothing.
    assert!(
        converted_to_drop > 50,
        "cap only converted {converted_to_drop} verdicts to Drop; it must \
         actually bite for this test to mean anything"
    );
}

// ── Bottleneck: queue first, drop only when full ────────────────────────────

/// Offer `n` datagrams of `len` bytes at `every_us` intervals and report, per
/// datagram, the queuing delay it incurred (`at_us - now_us`) or `None` if it
/// was dropped.
fn offer(profile: NetProfile, len: usize, n: usize, every_us: u64) -> Vec<Option<u64>> {
    let mut sim = NetSim::new(profile, 0xB077_1E00);
    (0..n)
        .map(|i| {
            let now = i as u64 * every_us;
            match sim.decide(len, now) {
                Verdict::Deliver { at_us } => Some(at_us - now),
                Verdict::Drop => None,
                other => panic!("unexpected verdict {other:?}"),
            }
        })
        .collect()
}

/// A link offered less than it can carry must add no queuing delay at all.
#[test]
fn an_under_loaded_bottleneck_adds_no_delay() {
    // 100 kB/s link, 1000-byte datagrams every 20 ms = 50 kB/s offered.
    let p = NetProfile {
        cap: CapTimeline::constant(100_000),
        bottleneck: Some(Bottleneck {
            depth_ms: 200,
            aqm: None,
        }),
        ..NetProfile::perfect()
    };
    let delays = offer(p, 1_000, 40, 20_000);
    assert!(delays.iter().all(|d| d.is_some()), "nothing should drop");
    // Only its own serialisation time: 1000 bytes at 100 kB/s = 10 ms.
    assert!(
        delays.iter().flatten().all(|&d| d <= 10_000),
        "under-loaded link must not build a queue: {delays:?}"
    );
}

/// The property the whole model exists for: an over-loaded link must show
/// *growing* delay, because that is the signal a delay-based congestion
/// controller estimates from. A dropping-only cap shows none of this.
#[test]
fn an_over_loaded_bottleneck_builds_queuing_delay_before_it_drops() {
    // 100 kB/s link, 1000-byte datagrams every 5 ms = 200 kB/s offered: 2x.
    let p = NetProfile {
        cap: CapTimeline::constant(100_000),
        bottleneck: Some(Bottleneck {
            depth_ms: 500,
            aqm: None,
        }),
        ..NetProfile::perfect()
    };
    let delays = offer(p, 1_000, 60, 5_000);
    let seen: Vec<u64> = delays.iter().flatten().copied().collect();
    assert!(seen.len() >= 2, "some datagrams must get through");
    assert!(
        seen.last().unwrap() > seen.first().unwrap(),
        "queuing delay must grow under sustained overload: {seen:?}"
    );
    // And loss must be the tail event, not the first one: the earliest
    // datagrams queue rather than vanish.
    assert!(
        delays[..10].iter().all(|d| d.is_some()),
        "early datagrams must queue, not drop: {:?}",
        &delays[..10]
    );
}

/// Bufferbloat: a deep buffer with no AQM should reach hundreds of
/// milliseconds of delay while barely dropping anything. This is the LTE/WiFi
/// case, and the one where a loss-based-only controller sees nothing wrong.
#[test]
fn a_deep_buffer_bloats_delay_instead_of_dropping() {
    let p = NetProfile {
        cap: CapTimeline::constant(100_000),
        bottleneck: Some(Bottleneck::lte_bufferbloat()),
        ..NetProfile::perfect()
    };
    let delays = offer(p, 1_000, 80, 5_000);
    let dropped = delays.iter().filter(|d| d.is_none()).count();
    let max_delay = delays.iter().flatten().copied().max().unwrap_or(0);
    assert!(
        max_delay > 100_000,
        "a 600 ms buffer must bloat past 100 ms of delay, saw {max_delay}us"
    );
    assert!(
        dropped * 4 < delays.len(),
        "bufferbloat drops little: {dropped} of {}",
        delays.len()
    );
}

/// AQM is the difference between the fibre case and the LTE case: under the
/// same offered load, CoDel must hold delay far below what a deep unmanaged
/// buffer reaches, by dropping earlier instead.
#[test]
fn aqm_holds_delay_down_where_a_deep_buffer_does_not() {
    let load = |bn: Bottleneck| -> (u64, usize) {
        let p = NetProfile {
            cap: CapTimeline::constant(100_000),
            bottleneck: Some(bn),
            ..NetProfile::perfect()
        };
        let d = offer(p, 1_000, 120, 5_000);
        (
            d.iter().flatten().copied().max().unwrap_or(0),
            d.iter().filter(|x| x.is_none()).count(),
        )
    };
    let (bloat_delay, bloat_drops) = load(Bottleneck::lte_bufferbloat());
    let (aqm_delay, aqm_drops) = load(Bottleneck::fibre_aqm());

    assert!(
        aqm_delay < bloat_delay,
        "AQM must hold delay below an unmanaged deep buffer: aqm={aqm_delay}us \
         bloat={bloat_delay}us"
    );
    assert!(
        aqm_drops > bloat_drops,
        "AQM trades loss for latency, so it must drop more: aqm={aqm_drops} \
         bloat={bloat_drops}"
    );
}

/// The queue must drain: once the burst stops, delay returns to baseline.
#[test]
fn the_queue_drains_after_a_burst() {
    let p = NetProfile {
        cap: CapTimeline::constant(100_000),
        bottleneck: Some(Bottleneck {
            depth_ms: 500,
            aqm: None,
        }),
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(p, 7);
    // Burst: 30 datagrams in 150 ms, far above the link rate.
    let mut peak = 0u64;
    for i in 0..30u64 {
        let now = i * 5_000;
        if let Verdict::Deliver { at_us } = sim.decide(1_000, now) {
            peak = peak.max(at_us - now);
        }
    }
    // The burst must actually have built a queue, or "it drained" is vacuous
    // — this assertion is what makes the drain below mean something.
    assert!(
        peak > 50_000,
        "the burst must build a real queue first, peak was {peak}us"
    );

    // Then idle for two seconds and offer one more.
    match sim.decide(1_000, 2_150_000) {
        Verdict::Deliver { at_us } => {
            let q = at_us - 2_150_000;
            assert!(q <= 10_000, "queue must have drained, saw {q}us");
        }
        other => panic!("expected delivery after drain, got {other:?}"),
    }
}
