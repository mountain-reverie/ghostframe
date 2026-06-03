//! Pure escalation-candidate detector for M3.3c idle-tile refinement.
//!
//! Identifies tiles that should escalate to CDF 5/3 refinement: tile is
//! idle (idle_frames > 30), currently displayed via a *lossy* snapshot
//! (codec_state ∈ {H264, Bc1}), and hasn't already been escalated this
//! generation. Returns up to `k_max` tile-idx values in row-major order.
//! No I/O, no GPU — unit-testable in isolation.
//!
//! **L1 prune (M3.5b, per docs/specs/m3-codec-bench-results.md §8):**
//! `PalRle` and `Solid` were removed from the source-codec set. Those
//! states are emitted only when the classifier's feasibility predicate
//! holds (≤16 colors / single-color tile), in which case the rendered
//! canvas already shows byte-exact content. Escalating them ran the
//! CDF53 forward transform + scheduler queueing for zero perceptual
//! gain — quantified at ~720 KB/s in the M3.5b mode_switch scene.

use crate::tile::{CodecState, MetricsTracker, TileMetrics};

/// Threshold matching the spec: tile must be idle for > IDLE_THRESHOLD frames
/// before becoming eligible for escalation. 30 frames ≈ 500 ms at 60 fps.
pub const IDLE_THRESHOLD: u32 = 30;

/// Returns up to `k_max` tile indices (`y * cols + x`) eligible for
/// idle-escalation, in row-major order. See module docs for eligibility.
pub fn detect_escalation_candidates(
    metrics: &MetricsTracker,
    k_max: usize,
) -> Vec<u32> {
    let mut out = Vec::new();
    for (idx, m) in metrics.metrics().iter().enumerate() {
        if out.len() >= k_max {
            break;
        }
        if is_eligible(m) {
            out.push(idx as u32);
        }
    }
    out
}

fn is_eligible(m: &TileMetrics) -> bool {
    if m.idle_frames <= IDLE_THRESHOLD {
        return false;
    }
    if m.already_escalated_this_gen {
        return false;
    }
    // L1 set post-M3.5b prune: only states that leave a lossy snapshot
    // on the client canvas. PalRle (with feasible ≤16-color palette) and
    // Solid (single-color) are already byte-exact — no refinement needed.
    matches!(
        m.codec_state,
        CodecState::H264 { .. } | CodecState::Bc1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker(rows: u32, cols: u32) -> MetricsTracker {
        MetricsTracker::new(cols, rows)
    }

    #[test]
    fn empty_tracker_yields_no_candidates() {
        let t = make_tracker(2, 2);
        assert!(detect_escalation_candidates(&t, 100).is_empty());
    }

    #[test]
    fn idle_h264_below_threshold_not_eligible() {
        let mut t = make_tracker(2, 2);
        let m = t.get_mut(0, 0);
        m.idle_frames = 30; // == threshold, not >
        m.codec_state = CodecState::H264 { frames_in_h264: 5 };
        assert!(detect_escalation_candidates(&t, 100).is_empty());
    }

    #[test]
    fn idle_h264_above_threshold_eligible() {
        let mut t = make_tracker(2, 2);
        let m = t.get_mut(0, 0);
        m.idle_frames = 31;
        m.codec_state = CodecState::H264 { frames_in_h264: 5 };
        assert_eq!(detect_escalation_candidates(&t, 100), vec![0]);
    }

    #[test]
    fn cdf53_already_in_refinement_not_eligible() {
        let mut t = make_tracker(2, 2);
        let m = t.get_mut(0, 0);
        m.idle_frames = 100;
        m.codec_state = CodecState::Cdf53 { passes_sent: 0, max_passes: 14 };
        assert!(detect_escalation_candidates(&t, 100).is_empty());
    }

    #[test]
    fn pixel_perfect_not_eligible() {
        let mut t = make_tracker(2, 2);
        let m = t.get_mut(0, 0);
        m.idle_frames = 100;
        m.codec_state = CodecState::PixelPerfect;
        assert!(detect_escalation_candidates(&t, 100).is_empty());
    }

    /// L1 prune (M3.5b): PalRle and Solid are lossless within their
    /// feasibility predicates and therefore must NOT escalate.
    #[test]
    fn lossless_sources_not_eligible() {
        let mut t = make_tracker(2, 2);
        let palrle = t.get_mut(0, 0);
        palrle.idle_frames = 100;
        palrle.codec_state = CodecState::PalRle { palette_id: 7 };
        let solid = t.get_mut(1, 0);
        solid.idle_frames = 100;
        solid.codec_state = CodecState::Solid;
        assert!(detect_escalation_candidates(&t, 100).is_empty(),
            "PalRle and Solid are lossless within feasibility — must NOT escalate");
    }

    #[test]
    fn already_escalated_flag_prevents_reentry() {
        let mut t = make_tracker(2, 2);
        let m = t.get_mut(0, 0);
        m.idle_frames = 100;
        m.codec_state = CodecState::H264 { frames_in_h264: 5 };
        m.already_escalated_this_gen = true;
        assert!(detect_escalation_candidates(&t, 100).is_empty());
    }

    #[test]
    fn k_max_caps_returned_candidates() {
        let mut t = make_tracker(4, 4); // 16 tiles
        for y in 0..4 {
            for x in 0..4 {
                let m = t.get_mut(x, y);
                m.idle_frames = 100;
                m.codec_state = CodecState::H264 { frames_in_h264: 5 };
            }
        }
        let v = detect_escalation_candidates(&t, 5);
        assert_eq!(v.len(), 5);
        // Row-major order: first 5 indices are 0..5.
        assert_eq!(v, vec![0, 1, 2, 3, 4]);
    }

    /// Post-M3.5b-prune source-codec set: H264 + Bc1 (Bc1 removal pending).
    #[test]
    fn both_remaining_lossy_sources_eligible() {
        let mut t = make_tracker(2, 2);
        let states = [
            CodecState::H264 { frames_in_h264: 1 },
            CodecState::Bc1,
        ];
        for (i, s) in states.iter().enumerate() {
            let x = (i % 2) as u32;
            let y = (i / 2) as u32;
            let m = t.get_mut(x, y);
            m.idle_frames = 100;
            m.codec_state = *s;
        }
        let v = detect_escalation_candidates(&t, 100);
        assert_eq!(v.len(), 2);
    }
}
