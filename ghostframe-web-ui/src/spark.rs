//! Sparkline geometry: pure arithmetic from a history to points in a
//! `WIDTH` x `HEIGHT` box. No DOM, so `cargo test` runs it on the host.

use ghostframe_client_core::stats::RING_LEN;

pub const WIDTH: f64 = 120.0;
pub const HEIGHT: f64 = 24.0;
const PAD: f64 = 2.0;

/// Points for `history` (oldest first), scaled to its own min and max. The
/// window is `RING_LEN` samples wide and right-aligned, so a young series
/// grows in from the right the way time does.
///
/// * empty (or no finite sample): no points, nothing is drawn;
/// * flat (all samples equal, or one sample): a midline across the box;
/// * otherwise: the polyline, `max` at the top and `min` at the bottom.
pub fn sparkline_points(history: &[f64], width: f64, height: f64) -> Vec<(f64, f64)> {
    let finite: Vec<f64> = history.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return Vec::new();
    }
    let min = finite.iter().copied().fold(f64::INFINITY, f64::min);
    let max = finite.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mid = height / 2.0;
    if finite.len() < 2 || max - min <= f64::EPSILON * max.abs().max(1.0) {
        return vec![(0.0, mid), (width, mid)];
    }
    let n = finite.len().min(RING_LEN);
    let finite = &finite[finite.len() - n..];
    let step = width / (RING_LEN - 1) as f64;
    let first = (RING_LEN - n) as f64 * step;
    let inner = height - 2.0 * PAD;
    finite
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = first + i as f64 * step;
            let y = PAD + (1.0 - (v - min) / (max - min)) * inner;
            (x, y)
        })
        .collect()
}

/// The SVG `points` attribute for `pts`: `"x,y x,y ..."`, one decimal.
pub fn points_attr(pts: &[(f64, f64)]) -> String {
    pts.iter()
        .map(|(x, y)| format!("{x:.1},{y:.1}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_series_draws_nothing() {
        assert!(sparkline_points(&[], WIDTH, HEIGHT).is_empty());
        assert!(sparkline_points(&[f64::NAN], WIDTH, HEIGHT).is_empty());
        assert_eq!(points_attr(&[]), "");
    }

    #[test]
    fn flat_and_single_series_draw_a_midline() {
        let mid = HEIGHT / 2.0;
        for h in [vec![5.0], vec![5.0, 5.0, 5.0], vec![0.0, 0.0]] {
            let p = sparkline_points(&h, WIDTH, HEIGHT);
            assert_eq!(p, vec![(0.0, mid), (WIDTH, mid)], "{h:?}");
        }
    }

    #[test]
    fn a_moving_series_spans_the_box_between_its_min_and_max() {
        let p = sparkline_points(&[1.0, 3.0, 2.0], WIDTH, HEIGHT);
        assert_eq!(p.len(), 3);
        let ys: Vec<f64> = p.iter().map(|(_, y)| *y).collect();
        assert_eq!(ys[0], HEIGHT - PAD, "min at the bottom");
        assert_eq!(ys[1], PAD, "max at the top");
        assert!(ys[2] > ys[1] && ys[2] < ys[0], "between");
        assert!(p.windows(2).all(|w| w[0].0 < w[1].0), "x increases");
    }

    #[test]
    fn the_window_is_right_aligned_and_full_series_span_the_width() {
        let few = sparkline_points(&[1.0, 2.0], WIDTH, HEIGHT);
        assert_eq!(
            few.last().map(|p| p.0),
            Some(WIDTH),
            "newest at the right edge"
        );
        assert!(few[0].0 > 0.0, "a young series does not start at the left");
        let full: Vec<f64> = (0..RING_LEN).map(|i| i as f64).collect();
        let p = sparkline_points(&full, WIDTH, HEIGHT);
        assert_eq!(p.first().map(|p| p.0), Some(0.0));
        assert_eq!(p.last().map(|p| p.0), Some(WIDTH));
    }

    #[test]
    fn non_finite_samples_are_skipped() {
        let p = sparkline_points(&[1.0, f64::NAN, 3.0], WIDTH, HEIGHT);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn attribute_has_one_decimal_per_coordinate() {
        assert_eq!(
            points_attr(&[(0.0, 22.0), (120.0, 2.04)]),
            "0.0,22.0 120.0,2.0"
        );
    }
}
