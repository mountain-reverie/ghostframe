use super::*;

fn h264_states(n: u32) -> Vec<CodecState> {
    (0..n)
        .map(|i| CodecState::H264 { frames_in_h264: i })
        .collect()
}

fn solid_states(n: u32) -> Vec<CodecState> {
    vec![CodecState::Solid; n as usize]
}

/// Compute the frame index (0-based) at which a wall-clock dwell of `dwell_us`
/// first elapses when frames arrive at `frame_interval_us` apart and the timer
/// starts at frame 0 (t=0). The switch fires when `frame_idx * frame_interval_us
/// >= dwell_us`, i.e. `ceil(dwell_us / frame_interval_us)`.
fn switch_frame(dwell_us: u64, frame_interval_us: u64) -> u32 {
    dwell_us.div_ceil(frame_interval_us) as u32
}

#[test]
fn enters_h264_after_sustain_frames_via_motion_fastpath() {
    let mut c = Classifier::default();
    let states = h264_states(20); // 20 dirty tiles, all H264 → 100% > 20%, ≥ 8 absolute
    // enter_sustain_micros = 50_000 µs ≈ 3 frames at 60 fps (16_667 µs/frame).
    // Frames 0..switch_frame-1 must return TileCodec; frame switch_frame returns H264.
    let sw = switch_frame(c.enter_sustain_micros, 16_667); // 3
    for frame in 0..sw {
        assert_eq!(
            c.decide_frame_mode_at((frame as u64) * 16_667, &states, FrameMode::TileCodec),
            FrameMode::TileCodec,
            "should not enter before sustain elapsed (frame {frame})",
        );
    }
    assert_eq!(
        c.decide_frame_mode_at((sw as u64) * 16_667, &states, FrameMode::TileCodec),
        FrameMode::H264
    );
}

#[test]
fn motion_fastpath_blocked_by_min_absolute_floor() {
    let mut c = Classifier::default();
    let states = h264_states(2); // only 2 H264 tiles — 100% but below floor of 8
    // Time can advance freely; floor means enter_now is always false → no switch.
    for frame in 0..10u32 {
        assert_eq!(
            c.decide_frame_mode_at((frame as u64) * 16_667, &states, FrameMode::TileCodec),
            FrameMode::TileCodec,
            "min-absolute floor must prevent fast-path on tiny dirty sets",
        );
    }
}

#[test]
fn enter_streak_resets_on_quiet_frame() {
    let mut c = Classifier::default();
    let busy = h264_states(20);
    let quiet = solid_states(1);
    // Build dwell to just before the switch point (sw-1 frames at 60 fps < dwell threshold).
    // enter_sustain_micros = 50_000; switch_frame = 3.
    let sw = switch_frame(c.enter_sustain_micros, 16_667); // 3
    let mut t = 0u64;
    for _ in 0..(sw - 1) {
        c.decide_frame_mode_at(t, &busy, FrameMode::TileCodec);
        t += 16_667;
    }
    // A quiet frame resets the enter timer (enter_started_us becomes None).
    c.decide_frame_mode_at(t, &quiet, FrameMode::TileCodec);
    t += 16_667;
    // First busy frame after reset: enter timer restarted at this t.
    // now_us - started_us = 0 (same frame) < dwell → must still be TileCodec.
    assert_eq!(
        c.decide_frame_mode_at(t, &busy, FrameMode::TileCodec),
        FrameMode::TileCodec
    );
}

#[test]
fn exits_h264_after_sustain_frames_when_costs_drop() {
    let mut c = Classifier::default();
    let cheap = solid_states(2); // tile-codec cost trivial → < h264 * 0.6
    // exit_sustain_micros = 500_000 µs; switch_frame at 60 fps = ceil(500_000/16_667) = 30.
    let sw = switch_frame(c.exit_sustain_micros, 16_667); // 30
    for frame in 0..sw {
        assert_eq!(
            c.decide_frame_mode_at((frame as u64) * 16_667, &cheap, FrameMode::H264),
            FrameMode::H264,
            "should not exit before sustain elapsed (frame {frame})",
        );
    }
    assert_eq!(
        c.decide_frame_mode_at((sw as u64) * 16_667, &cheap, FrameMode::H264),
        FrameMode::TileCodec
    );
}

#[test]
fn deadband_keeps_h264_mode_between_thresholds() {
    let mut c = Classifier::default();
    // Inflate exit_factor to make all our tile-codec costs land in the deadband.
    c.exit_factor = 0.0001;
    let states = h264_states(4); // some cost, but exit_factor makes exit unreachable
    // time can advance freely; exit_now is always false → stays H264.
    for frame in 0..50u32 {
        assert_eq!(
            c.decide_frame_mode_at((frame as u64) * 16_667, &states, FrameMode::H264),
            FrameMode::H264
        );
    }
}

#[test]
fn enters_h264_after_sustain_frames_via_cost_path_only() {
    // 200 Cdf53 tiles: cost = 200*90 µs + 200*1300 B/12.5 B/µs = 18_000 + 20_800 = 38_800 µs.
    // > h264_cost (3960) × enter_factor (1.3) = 5148 → cost_enter = true.
    // h264_tile_count = 0 < motion_tile_min_absolute (8) → motion_enter = false.
    // This is the cost-only enter path — exercises a code branch the other
    // enter-path tests don't reach (they hit motion_enter simultaneously).
    let mut c = Classifier::default();
    let states = vec![
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        };
        200
    ];
    // enter_sustain_micros = 50_000 µs; switch_frame at 60 fps = 3.
    let sw = switch_frame(c.enter_sustain_micros, 16_667); // 3
    for frame in 0..sw {
        assert_eq!(
            c.decide_frame_mode_at((frame as u64) * 16_667, &states, FrameMode::TileCodec),
            FrameMode::TileCodec,
            "should not enter before sustain elapsed (frame {frame})",
        );
    }
    assert_eq!(
        c.decide_frame_mode_at((sw as u64) * 16_667, &states, FrameMode::TileCodec),
        FrameMode::H264
    );
}

#[test]
fn empty_tentative_in_h264_drives_exit_streak_to_completion() {
    let mut c = Classifier::default();
    // Pre-establish H264 mode by feeding enough enter-triggering frames.
    // enter_sustain_micros = 50_000 µs; switch_frame at 60 fps = 3.
    let busy = h264_states(20);
    let enter_sw = switch_frame(c.enter_sustain_micros, 16_667); // 3
    let mut t = 0u64;
    for _ in 0..=enter_sw {
        c.decide_frame_mode_at(t, &busy, FrameMode::TileCodec);
        t += 16_667;
    }
    // Now classifier has seen H264 mode — caller would have set self.frame_mode = H264.

    // Simulate frames with no dirty tiles (static content).
    // exit_sustain_micros = 500_000 µs; switch_frame at 60 fps = 30.
    let empty: Vec<CodecState> = Vec::new();
    let exit_sw = switch_frame(c.exit_sustain_micros, 16_667); // 30
    let exit_t0 = t;
    for frame in 0..exit_sw {
        assert_eq!(
            c.decide_frame_mode_at(exit_t0 + (frame as u64) * 16_667, &empty, FrameMode::H264),
            FrameMode::H264,
            "should not exit before sustain elapsed (frame {frame})",
        );
    }
    assert_eq!(
        c.decide_frame_mode_at(exit_t0 + (exit_sw as u64) * 16_667, &empty, FrameMode::H264),
        FrameMode::TileCodec,
        "empty dirty tiles for exit_sustain_micros must flip H264 → TileCodec",
    );
}

#[test]
fn motion_fastpath_at_min_absolute_boundary() {
    // n=7 (just below floor=8): must NOT trip even at 100% motion fraction.
    let mut c = Classifier::default();
    let n7 = h264_states(7);
    for frame in 0..10u32 {
        assert_eq!(
            c.decide_frame_mode_at((frame as u64) * 16_667, &n7, FrameMode::TileCodec),
            FrameMode::TileCodec
        );
    }
    // n=8 (at floor): must trip after sustain.
    // enter_sustain_micros = 50_000 µs; switch_frame at 60 fps = 3.
    let mut c = Classifier::default();
    let n8 = h264_states(8);
    let sw = switch_frame(c.enter_sustain_micros, 16_667); // 3
    for frame in 0..sw {
        assert_eq!(
            c.decide_frame_mode_at((frame as u64) * 16_667, &n8, FrameMode::TileCodec),
            FrameMode::TileCodec,
            "should not enter before sustain elapsed (frame {frame})",
        );
    }
    assert_eq!(
        c.decide_frame_mode_at((sw as u64) * 16_667, &n8, FrameMode::TileCodec),
        FrameMode::H264
    );
}

#[test]
fn empty_tentative_in_tilecodec_stays_tilecodec() {
    let mut c = Classifier::default();
    let empty: Vec<CodecState> = Vec::new();
    for frame in 0..100u32 {
        assert_eq!(
            c.decide_frame_mode_at((frame as u64) * 16_667, &empty, FrameMode::TileCodec),
            FrameMode::TileCodec,
            "no input should never trigger H264 entry",
        );
    }
}

#[test]
fn refinement_bias_promotes_tilecodec_under_headroom() {
    use crate::tile::classifier::{AdaptationContext, Classifier, REFINEMENT_BIAS_PER_TILE_US};
    use crate::tile::{CodecState, FrameMode};

    // At 100 B/µs bandwidth, 40 Cdf53 tiles produce:
    //   tile_codec_cost = 40 * 90.0 + 40 * 1300 / 100.0 = 3600 + 520 = 4120 µs
    //   h264_cost (no bias) = 3000 + 12000/100 = 3120 µs
    //   enter threshold (no bias) = 3120 * 1.3 = 4056 µs  → 4120 > 4056 → enters H264
    //
    //   With deficit=40 and REFINEMENT_BIAS_PER_TILE_US=5.0:
    //   h264_cost (with bias) = 3120 + 40*5 = 3320 µs
    //   enter threshold (with bias) = 3320 * 1.3 = 4316 µs → 4120 ≤ 4316 → stays TileCodec
    //
    // Plenty of bandwidth: 100 B/µs (≈ 800 Mbps).
    let ctx = AdaptationContext {
        bytes_per_us: 100.0,
        smoothed_rtt_us: 5_000.0,
        loss_rate: 0.0,
        suspended: false,
        last_update_seq: 1,
    };
    let tentative: Vec<CodecState> = (0..40)
        .map(|_| CodecState::Cdf53 {
            passes_sent: 3,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        })
        .collect();

    // With bias: even well past enter_sustain_micros, cost_enter is false
    // (tile_codec_cost ≤ h264_cost_biased * enter_factor), so TileCodec is held.
    // Drive 10 frames (≈ 166_670 µs >> 50_000 µs dwell) to ensure no switch occurs.
    let mut c = Classifier::default();
    c.set_adaptation_context(ctx);
    c.set_refinement_deficit_tiles(40);
    let mut mode = FrameMode::TileCodec;
    for frame in 0..10u32 {
        mode = c.decide_frame_mode_at((frame as u64) * 16_667, &tentative, mode);
    }
    assert_eq!(mode, FrameMode::TileCodec, "bias should hold TileCodec");

    // Without bias: tile_codec_cost > h264_cost * enter_factor → H264 after sustain.
    // enter_sustain_micros = 50_000 µs; switch_frame at 60 fps = 3.
    // Drive to frame switch_frame(50_000, 16_667) = 3 to confirm H264.
    let sw = switch_frame(50_000, 16_667); // 3
    let mut c2 = Classifier::default();
    c2.set_adaptation_context(ctx);
    c2.set_refinement_deficit_tiles(0);
    let mut mode2 = FrameMode::TileCodec;
    for frame in 0..=sw {
        mode2 = c2.decide_frame_mode_at((frame as u64) * 16_667, &tentative, mode2);
    }
    assert_eq!(
        mode2,
        FrameMode::H264,
        "without deficit, cost comparison picks H264 (bias is zero)"
    );

    // Sanity: bias > 0 used.
    assert!(REFINEMENT_BIAS_PER_TILE_US > 0.0);
}

#[test]
fn headroom_guard_forces_h264_below_threshold() {
    use crate::tile::classifier::{
        AdaptationContext, Classifier, HEADROOM_MIN_BYTES_PER_US,
    };
    use crate::tile::{CodecState, FrameMode};
    let mut c = Classifier::default();
    // Below threshold: 0.1 B/µs (~ 800 kbps).
    let ctx = AdaptationContext {
        bytes_per_us: HEADROOM_MIN_BYTES_PER_US * 0.5,
        smoothed_rtt_us: 50_000.0,
        loss_rate: 0.0,
        suspended: false,
        last_update_seq: 1,
    };
    c.set_adaptation_context(ctx);
    // Use a "would normally pick TileCodec" workload: 1 Solid tile.
    let tentative = vec![CodecState::Solid];
    let mode = c.decide_frame_mode(&tentative, FrameMode::TileCodec);
    assert_eq!(mode, FrameMode::H264, "headroom guard should override");
}

#[test]
fn loss_override_forces_h264_above_threshold() {
    use crate::tile::classifier::{AdaptationContext, Classifier, LOSS_OVERRIDE_THRESHOLD};
    use crate::tile::{CodecState, FrameMode};
    let mut c = Classifier::default();
    let ctx = AdaptationContext {
        bytes_per_us: 100.0,
        smoothed_rtt_us: 5_000.0,
        loss_rate: LOSS_OVERRIDE_THRESHOLD + 0.05,
        suspended: false,
        last_update_seq: 1,
    };
    c.set_adaptation_context(ctx);
    let tentative = vec![CodecState::Solid];
    assert_eq!(
        c.decide_frame_mode(&tentative, FrameMode::TileCodec),
        FrameMode::H264
    );
}

#[test]
fn suspension_override_forces_h264() {
    use crate::tile::classifier::{AdaptationContext, Classifier};
    use crate::tile::{CodecState, FrameMode};
    let mut c = Classifier::default();
    let ctx = AdaptationContext {
        bytes_per_us: 100.0,
        smoothed_rtt_us: 5_000.0,
        loss_rate: 0.0,
        suspended: true,
        last_update_seq: 1,
    };
    c.set_adaptation_context(ctx);
    let tentative = vec![CodecState::Solid];
    assert_eq!(
        c.decide_frame_mode(&tentative, FrameMode::TileCodec),
        FrameMode::H264
    );
}

#[test]
fn hysteresis_micros_holds_dwell_across_frame_rate() {
    use crate::tile::classifier::Classifier;
    use crate::tile::{CodecState, FrameMode};
    // Construct a "force enter H264" workload: many tiles at high
    // Cdf53 cost. Reuses the Task 7 calibration: 40 tiles at the default
    // bandwidth pushes tile_codec_cost above enter_factor * h264_cost.
    let make_tentative = || -> Vec<CodecState> {
        let max_passes = crate::encoder::cdf53::CDF53_PASS_COUNT as u8;
        (0..40).map(|_| CodecState::Cdf53 { passes_sent: 0, max_passes }).collect()
    };

    let mut c60 = Classifier::default();
    let mut switch_at_60 = None;
    for frame in 0..1000u32 {
        let now_us = (frame as u64) * 16_667; // 60 fps
        let mode = c60.decide_frame_mode_at(now_us, &make_tentative(), FrameMode::TileCodec);
        if mode == FrameMode::H264 {
            switch_at_60 = Some(now_us);
            break;
        }
    }

    let mut c30 = Classifier::default();
    let mut switch_at_30 = None;
    for frame in 0..1000u32 {
        let now_us = (frame as u64) * 33_333; // 30 fps
        let mode = c30.decide_frame_mode_at(now_us, &make_tentative(), FrameMode::TileCodec);
        if mode == FrameMode::H264 {
            switch_at_30 = Some(now_us);
            break;
        }
    }

    let us60 = switch_at_60.expect("60fps must eventually switch");
    let us30 = switch_at_30.expect("30fps must eventually switch");
    let diff = if us60 > us30 { us60 - us30 } else { us30 - us60 };
    // Wall-clock dwell at 60fps and 30fps should match within one 30-fps frame.
    assert!(
        diff <= 33_333,
        "frame-rate-independent dwell broken: 60fps switched at {us60}us, 30fps at {us30}us, diff {diff}us"
    );
}

#[test]
fn mode_decision_event_emitted_only_on_transition() {
    use crate::tile::classifier::{AdaptationContext, Classifier};
    use crate::tile::{CodecState, FrameMode};

    // This is mostly a behavioral / no-panic guard: tracing events are
    // hard to capture without a subscriber. The functional check is that
    // last_emitted_mode is correctly debounced.

    let mut c = Classifier::default();
    let ctx = AdaptationContext {
        bytes_per_us: 100.0,
        smoothed_rtt_us: 5_000.0,
        loss_rate: 0.0,
        suspended: false,
        last_update_seq: 1,
    };
    c.set_adaptation_context(ctx);
    let tentative = vec![CodecState::Solid];

    // First call from default state: emits initial decision.
    let m1 = c.decide_frame_mode_at(1_000, &tentative, FrameMode::TileCodec);
    assert_eq!(c.last_emitted_mode(), Some(m1));

    // Same conditions: no transition, no new emission, last stays the same.
    let m2 = c.decide_frame_mode_at(2_000, &tentative, m1);
    assert_eq!(m1, m2);
    assert_eq!(c.last_emitted_mode(), Some(m2));

    // Now force a transition via suspension override.
    let mut ctx2 = ctx;
    ctx2.suspended = true;
    c.set_adaptation_context(ctx2);
    let m3 = c.decide_frame_mode_at(3_000, &tentative, m2);
    assert_eq!(m3, FrameMode::H264);
    assert_eq!(c.last_emitted_mode(), Some(FrameMode::H264));
}

#[test]
fn env_var_override_refinement_bias_us() {
    use crate::tile::classifier::{AdaptationContext, Classifier, REFINEMENT_BIAS_PER_TILE_US};
    use crate::tile::{CodecState, FrameMode};

    let ctx = AdaptationContext {
        bytes_per_us: 100.0,
        smoothed_rtt_us: 5_000.0,
        loss_rate: 0.0,
        suspended: false,
        last_update_seq: 1,
    };
    let max_passes = crate::encoder::cdf53::CDF53_PASS_COUNT as u8;
    let tentative: Vec<CodecState> = (0..40)
        .map(|_| CodecState::Cdf53 { passes_sent: 0, max_passes })
        .collect();

    // Helper: drive decide_frame_mode_at for 200 ms simulated wall clock at
    // 60 fps frame cadence, starting from TileCodec. Returns the final mode.
    // `deficit` is the refinement_deficit_tiles count to set each frame.
    let drive = |c: &mut Classifier, deficit: u32| -> FrameMode {
        c.set_adaptation_context(ctx);
        c.set_refinement_deficit_tiles(deficit);
        let mut mode = FrameMode::TileCodec;
        for frame in 0..12u32 {
            let now_us = (frame as u64) * 16_667;
            mode = c.decide_frame_mode_at(now_us, &tentative, mode);
        }
        mode
    };

    // Both cases use deficit=1. This is the critical parameter:
    //
    //   tile_codec_cost = 40*90 + 40*1300/100.0 = 3600 + 520 = 4120 µs
    //   h264_base = 3000 + 12000/100.0 = 3120 µs
    //
    //   Default bias (5.0 µs/tile), deficit=1:
    //     h264_cost = 3120 + 1*5.0 = 3125 µs
    //     enter threshold = 3125 * 1.3 = 4062.5 µs
    //     4120 > 4062.5 → cost_enter = true → H264 after dwell
    //
    //   Override (1000.0 µs/tile), deficit=1:
    //     h264_cost = 3120 + 1*1000.0 = 4120 µs
    //     enter threshold = 4120 * 1.3 = 5356 µs
    //     4120 ≤ 5356 → cost_enter = false → TileCodec holds

    // Case 1: no override. Default bias = 5.0 µs/tile, threshold = 4062 µs <
    // tile_codec_cost (4120 µs) → cost_enter = true → H264 after enter dwell.
    let mut c_default = Classifier::default();
    assert_eq!(
        drive(&mut c_default, 1),
        FrameMode::H264,
        "without override, default bias (5 µs × 1 tile = 5 µs) lets cost path flip to H264"
    );

    // Case 2: with override = 1000.0 µs/tile, threshold = 5356 µs >
    // tile_codec_cost (4120 µs) → cost_enter = false → TileCodec across the dwell.
    std::env::set_var("GHOSTFRAME_TEST_REFINEMENT_BIAS_US", "1000.0");
    let mut c_override = Classifier::default();
    std::env::remove_var("GHOSTFRAME_TEST_REFINEMENT_BIAS_US");
    assert_eq!(
        drive(&mut c_override, 1),
        FrameMode::TileCodec,
        "with override = 1000 µs/tile (1 tile), threshold 5356 µs > tile_codec_cost → TileCodec holds"
    );

    // Sanity: default constant unchanged.
    assert_eq!(REFINEMENT_BIAS_PER_TILE_US, 5.0);
}

#[test]
fn env_var_override_loss_override_threshold() {
    use crate::tile::classifier::{AdaptationContext, Classifier, LOSS_OVERRIDE_THRESHOLD};
    use crate::tile::{CodecState, FrameMode};

    // Raise the threshold to 0.50 — well above the test ctx's 0.15.
    std::env::set_var("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD", "0.50");
    let mut c = Classifier::default();
    std::env::remove_var("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD");

    let ctx = AdaptationContext {
        bytes_per_us: 100.0,
        smoothed_rtt_us: 5_000.0,
        loss_rate: 0.15,
        suspended: false,
        last_update_seq: 1,
    };
    c.set_adaptation_context(ctx);
    let tentative = vec![CodecState::Solid];

    // 0.15 < 0.50 ⇒ override should NOT fire (would fire at default 0.10).
    assert_eq!(
        c.decide_frame_mode(&tentative, FrameMode::TileCodec),
        FrameMode::TileCodec
    );
    assert_eq!(LOSS_OVERRIDE_THRESHOLD, 0.10);
}

#[test]
fn env_var_override_headroom_min_bpus() {
    use crate::tile::classifier::{AdaptationContext, Classifier, HEADROOM_MIN_BYTES_PER_US};
    use crate::tile::{CodecState, FrameMode};

    // Lower the threshold to 0.05 — well below the test ctx's 0.1.
    std::env::set_var("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS", "0.05");
    let mut c = Classifier::default();
    std::env::remove_var("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS");

    let ctx = AdaptationContext {
        bytes_per_us: 0.1,
        smoothed_rtt_us: 50_000.0,
        loss_rate: 0.0,
        suspended: false,
        last_update_seq: 1,
    };
    c.set_adaptation_context(ctx);
    let tentative = vec![CodecState::Solid];

    // 0.1 > 0.05 ⇒ headroom override should NOT fire (would fire at default 0.25).
    assert_eq!(
        c.decide_frame_mode(&tentative, FrameMode::TileCodec),
        FrameMode::TileCodec
    );
    assert_eq!(HEADROOM_MIN_BYTES_PER_US, 0.25);
}
