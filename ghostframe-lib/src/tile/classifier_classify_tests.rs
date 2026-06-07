use super::*;

fn metrics(freq: f32, mag: f32, idle: u32, unique_colors: u16) -> TileMetrics {
    TileMetrics {
        change_freq_hz: freq,
        change_magnitude: mag,
        idle_frames: idle,
        unique_colors,
        edge_density: super::super::EDGE_DENSITY_UNKNOWN,
        codec_state: CodecState::Skip,
        already_escalated_this_gen: false,
    }
}

#[test]
fn idle_tile_classified_as_skip() {
    let m = metrics(0.0, 0.0, 1, super::super::UNIQUE_COLORS_UNKNOWN);
    assert_eq!(classify_tile(&m, &CodecState::Solid), CodecState::Skip);
}

#[test]
fn high_freq_high_magnitude_picks_h264() {
    let m = metrics(60.0, 0.5, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    let next = classify_tile(&m, &CodecState::Skip);
    assert!(matches!(next, CodecState::H264 { .. }));
}

#[test]
fn h264_state_increments_frames_counter() {
    let m = metrics(60.0, 0.5, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    let next = classify_tile(&m, &CodecState::H264 { frames_in_h264: 7 });
    assert_eq!(next, CodecState::H264 { frames_in_h264: 8 });
}

#[test]
fn high_freq_low_magnitude_few_colors_picks_palrle() {
    let m = metrics(60.0, 0.05, 0, 4);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::PalRle { palette_id: 0 },
    );
}

#[test]
fn high_freq_low_magnitude_many_colors_picks_cdf53() {
    // Post-BC1-removal: Rule 3's high-color branch falls back to Cdf53
    // refinement rather than the old Bc1-as-Raw wire emission.
    let m = metrics(60.0, 0.05, 0, 200);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        },
    );
}

#[test]
fn high_freq_low_magnitude_sentinel_unique_colors_picks_cdf53() {
    let m = metrics(60.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        },
    );
}

#[test]
fn medium_freq_holds_h264_via_hysteresis() {
    let m = metrics(8.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    let next = classify_tile(&m, &CodecState::H264 { frames_in_h264: 3 });
    assert_eq!(next, CodecState::H264 { frames_in_h264: 4 });
}

#[test]
fn medium_freq_no_prior_h264_picks_cdf53() {
    let m = metrics(8.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        },
    );
}

#[test]
fn low_freq_single_color_picks_solid() {
    let m = metrics(2.0, 0.05, 0, 1);
    assert_eq!(classify_tile(&m, &CodecState::Skip), CodecState::Solid);
}

#[test]
fn low_freq_few_colors_picks_palrle() {
    let m = metrics(2.0, 0.05, 0, 8);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::PalRle { palette_id: 0 },
    );
}

#[test]
fn low_freq_sentinel_unique_colors_falls_through_to_cdf53() {
    let m = metrics(2.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    let next = classify_tile(&m, &CodecState::Skip);
    assert!(matches!(next, CodecState::Cdf53 { .. }));
}

#[test]
fn magnitude_at_threshold_routes_to_rule_3_not_rule_2() {
    // Rule 2 uses mag > 0.3 (strict); Rule 3 uses mag <= 0.3.
    // At exactly 0.3, the tile must NOT classify as H264.
    let m = metrics(60.0, 0.3, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    let next = classify_tile(&m, &CodecState::Skip);
    assert!(
        !matches!(next, CodecState::H264 { .. }),
        "mag = 0.3 must fall through to Rule 3 (Cdf53/PalRle), got {next:?}"
    );
}

#[test]
fn freq_at_15_lands_in_rule_4_range() {
    // Rules 2/3 use freq > 15.0 (strict); Rule 4 uses 5.0..=15.0 (inclusive).
    // freq = 15.0 must hit Rule 4 (Cdf53 fallback with no prev H264), not Rules 2/3.
    let m = metrics(15.0, 0.5, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        },
    );
}

#[test]
fn freq_at_5_lands_in_rule_4_range() {
    // Lower bound of Rule 4 is inclusive — freq = 5.0 must hit Rule 4.
    let m = metrics(5.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        },
    );
}

#[test]
fn first_h264_entry_starts_counter_at_one() {
    // Load-bearing for Task 5's sustain logic: the counter starts at 1, not 0.
    let m = metrics(60.0, 0.5, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::H264 { frames_in_h264: 1 },
    );
}

#[test]
fn unique_colors_16_picks_palrle() {
    let m = metrics(2.0, 0.05, 0, 16);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::PalRle { palette_id: 0 }
    );
}

#[test]
fn unique_colors_17_falls_through_to_cdf53() {
    let m = metrics(2.0, 0.05, 0, 17);
    assert!(matches!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::Cdf53 { .. }
    ));
}

#[test]
fn h264_tile_at_low_freq_falls_through_to_rules_6_to_8() {
    // freq < 5.0 with prev=H264: rules 6-8 apply normally, NOT H264 hysteresis.
    // Pins the doc-comment behavior — hysteresis only acts in the medium-freq band.
    let m = metrics(2.0, 0.05, 0, 1); // single color
    let next = classify_tile(&m, &CodecState::H264 { frames_in_h264: 99 });
    assert_eq!(
        next,
        CodecState::Solid,
        "doc-comment hysteresis claim is per-rule-4 only — at low freq, normal rules apply"
    );
}

#[test]
fn cdf53_fallback_uses_cdf53_pass_count() {
    // The fallback must use CDF53_PASS_COUNT (14) as max_passes, not a
    // hardcoded 9, so that the full pass budget is visible to the scheduler.
    let m = metrics(2.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    assert_eq!(
        classify_tile(&m, &CodecState::Skip),
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        },
    );
}

#[test]
fn medium_freq_with_single_color_prefers_solid_over_fallback() {
    // Rule 5 modification: medium freq + uc_known + 1 unique color ⇒ Solid,
    // not the Cdf53 fallback. Mirrors Rule 3's structure for high-freq + low-mag.
    let m = metrics(6.0, 0.05, 0, 1);
    let prev = CodecState::Cdf53 {
        passes_sent: 0,
        max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
    };
    assert_eq!(classify_tile(&m, &prev), CodecState::Solid);
}

#[test]
fn medium_freq_with_few_colors_prefers_palrle_over_fallback() {
    // Rule 5 modification: medium freq + uc_known + 2..16 unique colors ⇒ PalRle.
    let m = metrics(8.0, 0.10, 0, 8);
    let prev = CodecState::Cdf53 {
        passes_sent: 0,
        max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
    };
    assert_eq!(
        classify_tile(&m, &prev),
        CodecState::PalRle { palette_id: 0 }
    );
}

#[test]
fn medium_freq_with_many_colors_falls_back_to_cdf53() {
    // Rule 5 fallback: medium freq + uc_known + > 16 colors ⇒ Cdf53.
    let m = metrics(6.0, 0.05, 0, 100);
    let prev = CodecState::Cdf53 {
        passes_sent: 0,
        max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
    };
    assert_eq!(
        classify_tile(&m, &prev),
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        },
    );
}

#[test]
fn medium_freq_unknown_colors_falls_back_to_cdf53() {
    // Rule 5 fallback: medium freq + unknown colors ⇒ Cdf53.
    let m = metrics(6.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
    let prev = CodecState::Cdf53 {
        passes_sent: 0,
        max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
    };
    assert_eq!(
        classify_tile(&m, &prev),
        CodecState::Cdf53 {
            passes_sent: 0,
            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
        },
    );
}

#[test]
fn medium_freq_h264_hysteresis_takes_precedence_over_lossless() {
    // H264 hysteresis at medium freq must beat the new lossless-preference
    // (an in-motion tile briefly hitting unique_colors=1 shouldn't drop to
    // Solid mid-motion).
    let m = metrics(8.0, 0.05, 0, 1);
    let prev = CodecState::H264 { frames_in_h264: 7 };
    assert_eq!(
        classify_tile(&m, &prev),
        CodecState::H264 { frames_in_h264: 8 }
    );
}

#[test]
fn pixel_perfect_idle_returns_skip() {
    let mut m = TileMetrics::default();
    m.idle_frames = 100;
    m.codec_state = CodecState::Skip; // codec_state is the PREVIOUS state, not the input
    assert_eq!(
        classify_tile(&m, &CodecState::PixelPerfect),
        CodecState::Skip
    );
}

#[test]
fn pixel_perfect_dirty_overridden_by_normal_rules() {
    let mut m = TileMetrics::default();
    m.idle_frames = 0; // dirty
    m.change_freq_hz = 16.0;
    m.change_magnitude = 0.5;
    // Was PixelPerfect; now dirty + high-motion → H264.
    let result = classify_tile(&m, &CodecState::PixelPerfect);
    assert!(matches!(result, CodecState::H264 { .. }));
}
