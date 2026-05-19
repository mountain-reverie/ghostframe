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
            max_passes: 9
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
