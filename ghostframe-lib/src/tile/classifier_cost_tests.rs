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

#[test]
fn adaptation_context_overrides_static_bytes_per_us() {
    use crate::tile::classifier::{AdaptationContext, Classifier};
    let mut c = Classifier::default();
    let initial = c.cost.bytes_per_us();
    let ctx = AdaptationContext {
        supports_h264: true,
        bytes_per_us: initial * 2.0,
        smoothed_rtt_us: 18_500.0,
        loss_rate: 0.0,
        suspended: false,
        last_update_seq: 1,
    };
    c.set_adaptation_context(ctx);
    assert!((c.cost.bytes_per_us() - initial * 2.0).abs() < 1e-6);
    assert_eq!(c.adaptation_context().last_update_seq, 1);
}

#[test]
fn adaptation_context_with_zero_bytes_per_us_preserves_cost() {
    use crate::tile::classifier::{AdaptationContext, Classifier};
    let mut c = Classifier::default();
    let initial = c.cost.bytes_per_us();
    let ctx = AdaptationContext {
        supports_h264: true,
        bytes_per_us: 0.0,
        smoothed_rtt_us: 18_500.0,
        loss_rate: 0.0,
        suspended: false,
        last_update_seq: 1,
    };
    c.set_adaptation_context(ctx);
    assert!(
        (c.cost.bytes_per_us() - initial).abs() < 1e-6,
        "zero bytes_per_us must not overwrite cost"
    );
    // But the context snapshot itself still reflects the pushed value.
    assert_eq!(c.adaptation_context().last_update_seq, 1);
    assert_eq!(c.adaptation_context().bytes_per_us, 0.0);
}
