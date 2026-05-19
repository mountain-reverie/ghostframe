use super::*;

#[test]
fn default_is_self_consistent() {
    let c = CostModel::default();
    // H.264 should dominate per-tile codec costs.
    assert!(c.h264_frame_us > c.solid_us * 1000.0);
    assert!(c.bytes_per_us() > 0.0);
}

#[test]
fn skip_and_pixel_perfect_cost_zero() {
    let c = CostModel::default();
    assert_eq!(c.estimated_tile_us(CodecState::Skip), 0.0);
    assert_eq!(c.estimated_tile_us(CodecState::PixelPerfect), 0.0);
    assert_eq!(c.estimated_tile_bytes(CodecState::Skip), 0);
    assert_eq!(c.estimated_tile_bytes(CodecState::PixelPerfect), 0);
}
