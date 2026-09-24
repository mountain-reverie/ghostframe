//! The CPU reference for `h264_nv12_blit.wgsl`.
//!
//! Exists so the shader can be checked against something, and so the
//! constants live in exactly two places that a test compares. Both invert
//! `ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp`, which is
//! **full-range BT.601** -- see the design doc §5. Not BT.709. Not limited
//! range.

/// Chroma is centred on 128/255, matching the forward shader's `+ 0.502`.
pub const CHROMA_CENTRE: f32 = 0.502;

/// Exact inverse of the forward matrix. The textbook full-range BT.601
/// inverse differs from this by at most 0.171/255 across the whole YUV cube,
/// so the difference is invisible -- these are used because inverting the
/// transform the encoder actually applied is the correct thing to do.
pub const R_COEFF: [f32; 3] = [1.0, -0.000927, 1.401687];
pub const G_COEFF: [f32; 3] = [1.0, -0.343695, -0.714169];
pub const B_COEFF: [f32; 3] = [1.0, 1.772_16, 0.000990];

/// Convert one NV12 pixel to RGBA8.
///
/// `luma`/`chroma` are raw 8-bit samples. Chroma is upsampled
/// nearest-neighbour, not bilinear: the forward shader takes chroma from the
/// top-left pixel of each 2x2 block rather than averaging, so replicating
/// that sample is what inverts it.
pub fn nv12_pixel_to_rgba(luma: u8, cb: u8, cr: u8) -> [u8; 4] {
    let y = luma as f32 / 255.0;
    let u = cb as f32 / 255.0 - CHROMA_CENTRE;
    let v = cr as f32 / 255.0 - CHROMA_CENTRE;

    let r = R_COEFF[0] * y + R_COEFF[1] * u + R_COEFF[2] * v;
    let g = G_COEFF[0] * y + G_COEFF[1] * u + G_COEFF[2] * v;
    let b = B_COEFF[0] * y + B_COEFF[1] * u + B_COEFF[2] * v;

    [to_u8(r), to_u8(g), to_u8(b), 255]
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neutral chroma with full-range luma must round-trip to grey, at both
    /// ends. Under a LIMITED-range matrix, Y=0 would map to black only after
    /// a 16/255 pedestal subtraction and Y=255 would clip early -- so this
    /// test is what catches a well-meaning "fix" to BT.709 limited.
    #[test]
    fn full_range_luma_maps_to_the_full_grey_ramp() {
        assert_eq!(nv12_pixel_to_rgba(0, 128, 128), [0, 0, 0, 255]);
        assert_eq!(nv12_pixel_to_rgba(255, 128, 128), [255, 255, 255, 255]);
        let mid = nv12_pixel_to_rgba(128, 128, 128);
        assert!(
            (127..=129).contains(&mid[0]),
            "mid grey should stay mid grey, got {mid:?}"
        );
    }

    /// Inverting the forward shader on a known colour must return it. Red is
    /// the channel that the encoder-side R/B swap bug (fixed in 847d870) got
    /// wrong, so it is the one worth pinning.
    #[test]
    fn round_trips_pure_red_through_the_forward_matrix() {
        // Forward, from bgra_to_nv12.comp with R=1, G=0, B=0.
        let y = (0.299f32 * 255.0 + 0.5) as u8;
        let u = ((-0.169f32 + 0.502) * 255.0 + 0.5) as u8;
        let v = ((0.500f32 + 0.502) * 255.0 + 0.5) as u8;

        let rgba = nv12_pixel_to_rgba(y, u, v);
        assert!(rgba[0] > 250, "red channel should be ~255, got {rgba:?}");
        assert!(rgba[1] < 5, "green channel should be ~0, got {rgba:?}");
        assert!(rgba[2] < 5, "blue channel should be ~0, got {rgba:?}");
    }
}
