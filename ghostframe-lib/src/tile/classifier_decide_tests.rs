use super::*;

fn h264_states(n: u32) -> Vec<CodecState> {
    (0..n)
        .map(|i| CodecState::H264 { frames_in_h264: i })
        .collect()
}

fn solid_states(n: u32) -> Vec<CodecState> {
    vec![CodecState::Solid; n as usize]
}

#[test]
fn enters_h264_after_sustain_frames_via_motion_fastpath() {
    let mut c = Classifier::default();
    let states = h264_states(20); // 20 dirty tiles, all H264 → 100% > 20%, ≥ 8 absolute
    for _ in 0..(c.enter_sustain_frames - 1) {
        assert_eq!(
            c.decide_frame_mode(&states, FrameMode::TileCodec),
            FrameMode::TileCodec,
            "should not enter before sustain elapsed",
        );
    }
    assert_eq!(
        c.decide_frame_mode(&states, FrameMode::TileCodec),
        FrameMode::H264
    );
}

#[test]
fn motion_fastpath_blocked_by_min_absolute_floor() {
    let mut c = Classifier::default();
    let states = h264_states(2); // only 2 H264 tiles — 100% but below floor of 8
    for _ in 0..10 {
        assert_eq!(
            c.decide_frame_mode(&states, FrameMode::TileCodec),
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
    // Build streak almost to threshold...
    for _ in 0..(c.enter_sustain_frames - 1) {
        c.decide_frame_mode(&busy, FrameMode::TileCodec);
    }
    // ...then a quiet frame resets it.
    c.decide_frame_mode(&quiet, FrameMode::TileCodec);
    // First busy frame after reset should NOT promote us yet.
    assert_eq!(
        c.decide_frame_mode(&busy, FrameMode::TileCodec),
        FrameMode::TileCodec
    );
}

#[test]
fn exits_h264_after_sustain_frames_when_costs_drop() {
    let mut c = Classifier::default();
    let cheap = solid_states(2); // tile-codec cost trivial → < h264 * 0.6
    for _ in 0..(c.exit_sustain_frames - 1) {
        assert_eq!(
            c.decide_frame_mode(&cheap, FrameMode::H264),
            FrameMode::H264
        );
    }
    assert_eq!(
        c.decide_frame_mode(&cheap, FrameMode::H264),
        FrameMode::TileCodec
    );
}

#[test]
fn deadband_keeps_h264_mode_between_thresholds() {
    let mut c = Classifier::default();
    // Inflate exit_factor to make all our tile-codec costs land in the deadband.
    c.exit_factor = 0.0001;
    let states = h264_states(4); // some cost, but exit_factor makes exit unreachable
    for _ in 0..50 {
        assert_eq!(
            c.decide_frame_mode(&states, FrameMode::H264),
            FrameMode::H264
        );
    }
}

#[test]
fn enters_h264_after_sustain_frames_via_cost_path_only() {
    // 200 Cdf53 tiles: cost = 200*50 µs + 200*1300 B/12.5 B/µs = 30_800 µs.
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
    for _ in 0..(c.enter_sustain_frames - 1) {
        assert_eq!(
            c.decide_frame_mode(&states, FrameMode::TileCodec),
            FrameMode::TileCodec,
            "should not enter before sustain elapsed",
        );
    }
    assert_eq!(
        c.decide_frame_mode(&states, FrameMode::TileCodec),
        FrameMode::H264
    );
}

#[test]
fn empty_tentative_in_h264_drives_exit_streak_to_completion() {
    let mut c = Classifier::default();
    // Pre-establish H264 mode by feeding enough enter-triggering frames.
    let busy = h264_states(20);
    for _ in 0..c.enter_sustain_frames {
        c.decide_frame_mode(&busy, FrameMode::TileCodec);
    }
    // Now classifier is "in" H264 — caller would have set self.frame_mode = H264.

    // Simulate frames with no dirty tiles (static content).
    let empty: Vec<CodecState> = Vec::new();
    for _ in 0..(c.exit_sustain_frames - 1) {
        assert_eq!(
            c.decide_frame_mode(&empty, FrameMode::H264),
            FrameMode::H264,
            "should not exit before sustain elapsed",
        );
    }
    assert_eq!(
        c.decide_frame_mode(&empty, FrameMode::H264),
        FrameMode::TileCodec,
        "empty dirty tiles for exit_sustain frames must flip H264 → TileCodec",
    );
}

#[test]
fn motion_fastpath_at_min_absolute_boundary() {
    // n=7 (just below floor=8): must NOT trip even at 100% motion fraction.
    let mut c = Classifier::default();
    let n7 = h264_states(7);
    for _ in 0..10 {
        assert_eq!(
            c.decide_frame_mode(&n7, FrameMode::TileCodec),
            FrameMode::TileCodec
        );
    }
    // n=8 (at floor): must trip after sustain.
    let mut c = Classifier::default();
    let n8 = h264_states(8);
    for _ in 0..(c.enter_sustain_frames - 1) {
        assert_eq!(
            c.decide_frame_mode(&n8, FrameMode::TileCodec),
            FrameMode::TileCodec
        );
    }
    assert_eq!(
        c.decide_frame_mode(&n8, FrameMode::TileCodec),
        FrameMode::H264
    );
}

#[test]
fn empty_tentative_in_tilecodec_stays_tilecodec() {
    let mut c = Classifier::default();
    let empty: Vec<CodecState> = Vec::new();
    for _ in 0..100 {
        assert_eq!(
            c.decide_frame_mode(&empty, FrameMode::TileCodec),
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

    // With bias: even after enter_sustain_frames calls, cost_enter is false
    // (tile_codec_cost ≤ h264_cost_biased * enter_factor), so TileCodec is held.
    let mut c = Classifier::default();
    c.set_adaptation_context(ctx);
    c.set_refinement_deficit_tiles(40);
    let mut mode = FrameMode::TileCodec;
    for _ in 0..c.enter_sustain_frames {
        mode = c.decide_frame_mode(&tentative, mode);
    }
    assert_eq!(mode, FrameMode::TileCodec, "bias should hold TileCodec");

    // Without bias: tile_codec_cost > h264_cost * enter_factor → H264 after sustain.
    let mut c2 = Classifier::default();
    c2.set_adaptation_context(ctx);
    c2.set_refinement_deficit_tiles(0);
    let mut mode2 = FrameMode::TileCodec;
    for _ in 0..c2.enter_sustain_frames {
        mode2 = c2.decide_frame_mode(&tentative, mode2);
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
