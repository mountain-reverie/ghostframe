use super::*;
use crate::tile::{CodecState, TileMetrics, UNIQUE_COLORS_UNKNOWN};

const CDF53: CodecState = CodecState::Cdf53 {
    passes_sent: 0,
    present_passes: 0x3FFF,
};

#[test]
fn the_priors_price_each_state_until_something_is_observed() {
    let m = FirstPassModel::default();
    assert_eq!(m.state_bytes(CodecState::Skip), 0.0);
    assert_eq!(m.state_bytes(CodecState::PixelPerfect), 0.0);
    assert_eq!(m.state_bytes(CodecState::Solid), PRIOR_BYTES[0]);
    assert_eq!(
        m.state_bytes(CodecState::PalRle { palette_id: 3 }),
        PRIOR_BYTES[1]
    );
    assert_eq!(m.state_bytes(CDF53), PRIOR_BYTES[2]);
    // An H264-classified tile goes out as CDF 5/3 in tile mode.
    assert_eq!(
        m.state_bytes(CodecState::H264 { frames_in_h264: 4 }),
        PRIOR_BYTES[2]
    );
}

#[test]
fn calibration_converges_to_the_measured_size_per_tile() {
    let mut m = FirstPassModel::default();
    // 40 tiles of 500 B each, batch after batch.
    for _ in 0..200 {
        m.observe(FirstPassKind::Cdf53, 40 * 500, 40);
    }
    let got = m.state_bytes(CDF53);
    assert!((got - 500.0).abs() < 5.0, "{got}");
    // Other kinds are untouched.
    assert_eq!(m.state_bytes(CodecState::Solid), PRIOR_BYTES[0]);
}

#[test]
fn passes_split_across_batches_still_average_per_tile() {
    // A tile's critical passes may go out in a later batch than its pass 0:
    // bytes with no new tile, then a tile with fewer bytes. The ratio of the
    // two averages is still bytes per tile.
    let mut m = FirstPassModel::default();
    for _ in 0..400 {
        m.observe(FirstPassKind::Cdf53, 600, 1);
        m.observe(FirstPassKind::Cdf53, 400, 0);
    }
    // An EWMA of an alternating input oscillates around its mean; ending on
    // the low half leaves the ratio about 3 % high.
    let got = m.state_bytes(CDF53);
    assert!((got - 1000.0).abs() < 50.0, "{got}");
}

#[test]
fn an_empty_observation_changes_nothing() {
    let mut m = FirstPassModel::default();
    m.observe(FirstPassKind::PalRle, 0, 0);
    assert_eq!(
        m.state_bytes(CodecState::PalRle { palette_id: 0 }),
        PRIOR_BYTES[1]
    );
}

#[test]
fn the_window_is_bytes_per_second_over_the_last_second() {
    let mut w = DemandWindow::default();
    // 30 frames of 1000 B over one second: 30 kB/s = 240 kbit/s.
    let mut rate = 0;
    for i in 0..30u64 {
        rate = w.push(i * 33_333, 1000.0);
    }
    assert_eq!(rate, 240_000);
    // Two seconds later with nothing new, the old frames have left.
    assert_eq!(w.push(3_000_000, 0.0), 0);
}

#[test]
fn the_window_forgets_frames_older_than_one_second() {
    let mut w = DemandWindow::default();
    w.push(0, 10_000.0);
    // Exactly one window later the first frame is out.
    assert_eq!(w.push(DEMAND_WINDOW_US, 1000.0), 8_000);
}

#[test]
fn repaint_state_follows_the_colour_count() {
    let with = |unique_colors| TileMetrics {
        unique_colors,
        ..TileMetrics::default()
    };
    assert_eq!(repaint_state(&with(1)), CodecState::Solid);
    assert!(matches!(
        repaint_state(&with(16)),
        CodecState::PalRle { .. }
    ));
    assert!(matches!(repaint_state(&with(17)), CodecState::Cdf53 { .. }));
    assert!(matches!(
        repaint_state(&with(UNIQUE_COLORS_UNKNOWN)),
        CodecState::Cdf53 { .. }
    ));
}
