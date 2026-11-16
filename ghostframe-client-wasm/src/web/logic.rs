// SPDX-License-Identifier: MIT OR Apache-2.0
//! Port of `idle_skip.ts`, plus the two small choices `WebRenderer::create`
//! makes: which canvas format to present in and how the CDF 5/3 decode path is
//! chosen. Plain Rust: no browser needed to test them.

/// What `paint_tick` knows when it decides whether to touch the GPU.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaintInputs {
    /// A tile event, a resize, a session reset or a picture arrived since the
    /// last present.
    pub dirty: bool,
    /// H.264 pictures waiting to be drawn or dropped.
    pub h264_pending: usize,
    /// CDF 5/3 work carried over from the last flush (budget or throttle).
    pub deferred_work: bool,
    /// A PalRLE errors readback is in flight, or finished and not yet taken
    /// (`TileRenderer::errors_readback_pending`). The TypeScript rule was
    /// "in flight" only; a finished result must count too or a static screen
    /// never reports it.
    pub errors_readback_pending: bool,
}

/// True when the tick can return without encoding, submitting or presenting
/// anything: the canvas would show what it already shows.
pub fn should_skip_paint(i: &PaintInputs) -> bool {
    !(i.dirty || i.h264_pending > 0 || i.deferred_work || i.errors_readback_pending)
}

/// How CDF 5/3 tiles are decoded, from the page URL's `cdf53` parameter
/// (read by `app::page`, so this crate never touches the environment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cdf53Mode {
    /// The GPU grid if `TileRenderer::try_enable_cdf53_gpu`'s self-test passes, else the CPU decoder.
    Auto,
    /// The CPU decoder, without trying the grid (native: `GHOSTFRAME_CDF53_DECODE=cpu`). The
    /// e2e run that proves the fallback path on the web. (A mode that ran the self-test and
    /// then pretended it failed would need a way to disable an enabled grid, which
    /// `TileRenderer` does not have; the real failure branch is covered by PR 1's
    /// `the_self_test_keeps_a_grid_that_draws_nothing_off`.)
    Cpu,
}

impl Cdf53Mode {
    pub fn parse(s: &str) -> Self {
        match s {
            "cpu" => Cdf53Mode::Cpu,
            _ => Cdf53Mode::Auto,
        }
    }
}

/// The canvas format to present in. The page used `rgba8unorm` before (so the
/// copy from the framebuffer is byte-for-byte), and still prefers it; a canvas
/// that does not offer it gets the first non-sRGB format (an sRGB target would
/// re-encode the pixels), else the first one.
pub fn pick_canvas_format(offered: &[wgpu::TextureFormat]) -> wgpu::TextureFormat {
    use wgpu::TextureFormat::Rgba8Unorm;
    if offered.contains(&Rgba8Unorm) {
        return Rgba8Unorm;
    }
    offered
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .or_else(|| offered.first().copied())
        .unwrap_or(Rgba8Unorm)
}

/// `[count, p50, p99, max]` of `samples` (microseconds), for the bench's
/// `__ghostframePaintUs` and `__ghostframeTickUs`; zeros when empty.
pub fn summarize_us(mut samples: Vec<u32>) -> Vec<f64> {
    samples.sort_unstable();
    let at = |q: f64| {
        samples
            .get(((samples.len() as f64 - 1.0) * q).round() as usize)
            .copied()
            .map_or(0.0, f64::from)
    };
    vec![
        samples.len() as f64,
        at(0.5),
        at(0.99),
        samples.last().copied().map_or(0.0, f64::from),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_us_of_nothing_is_zeros() {
        assert_eq!(summarize_us(vec![]), vec![0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn summarize_us_sorts_and_rounds_the_percentile_index() {
        // p50 is index 1, p99 rounds 1.98 to index 2.
        assert_eq!(summarize_us(vec![5, 1, 3]), vec![3.0, 3.0, 5.0, 5.0]);
    }
    use wgpu::TextureFormat::{Bgra8Unorm, Bgra8UnormSrgb, Rgba8Unorm};

    #[test]
    fn skips_when_nothing_is_pending() {
        assert!(should_skip_paint(&PaintInputs::default()));
    }

    #[test]
    fn does_not_skip_for_any_one_reason() {
        for (name, i) in [
            (
                "dirty",
                PaintInputs {
                    dirty: true,
                    ..Default::default()
                },
            ),
            (
                "pictures",
                PaintInputs {
                    h264_pending: 1,
                    ..Default::default()
                },
            ),
            (
                "deferred",
                PaintInputs {
                    deferred_work: true,
                    ..Default::default()
                },
            ),
            (
                "readback",
                PaintInputs {
                    errors_readback_pending: true,
                    ..Default::default()
                },
            ),
        ] {
            assert!(!should_skip_paint(&i), "{name}");
        }
    }

    #[test]
    fn the_cdf53_mode_defaults_to_auto_and_names_the_one_override() {
        assert_eq!(Cdf53Mode::parse(""), Cdf53Mode::Auto);
        assert_eq!(Cdf53Mode::parse("auto"), Cdf53Mode::Auto);
        assert_eq!(Cdf53Mode::parse("cpu"), Cdf53Mode::Cpu);
        assert_eq!(
            Cdf53Mode::parse("fail"),
            Cdf53Mode::Auto,
            "the removed spelling must not pick an override"
        );
        assert_eq!(
            Cdf53Mode::parse("gpu?"),
            Cdf53Mode::Auto,
            "a typo must not silently pick an override"
        );
    }

    #[test]
    fn the_canvas_format_prefers_rgba8unorm_then_avoids_srgb() {
        assert_eq!(pick_canvas_format(&[Bgra8Unorm, Rgba8Unorm]), Rgba8Unorm);
        assert_eq!(
            pick_canvas_format(&[Bgra8UnormSrgb, Bgra8Unorm]),
            Bgra8Unorm
        );
        assert_eq!(pick_canvas_format(&[Bgra8UnormSrgb]), Bgra8UnormSrgb);
        assert_eq!(pick_canvas_format(&[]), Rgba8Unorm);
    }
}
