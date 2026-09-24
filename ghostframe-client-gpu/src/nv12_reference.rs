//! The CPU reference for `h264_nv12_blit.wgsl`.
//!
//! Exists so the shader can be checked against something, and so the
//! constants live in one Rust place that a test compares against the WGSL.
//! Both invert `ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp`, which
//! is **full-range BT.601** -- see the design doc §5. Not BT.709. Not
//! limited range. Counting the forward shader itself, the same six
//! coefficients and the `0.502` chroma centre appear in three places, not
//! two: this file, the WGSL, and the `.comp` shader they both invert. All
//! three must change together.
//!
//! This file has two conversion functions, and they model different halves
//! of the pipeline:
//!
//! - [`nv12_pixel_to_rgba`] is pure colour-matrix arithmetic on three
//!   already-selected samples. It does not know NV12 is a planar format and
//!   performs no plane indexing or chroma upsampling.
//! - [`nv12_plane_pixel_to_rgba`] models the actual NV12 planes -- luma and
//!   chroma byte buffers with their own strides -- and performs the
//!   nearest-neighbour chroma selection itself before calling the former.
//!   This is the one Task 8's oracle should compare against the shader,
//!   because the shader's `p / 2` chroma indexing is exactly as likely a
//!   source of a real bug as the matrix is, and a reference that never
//!   exercises indexing would "agree" with a wrong shader by construction.
//!
//! Both assume the unorm *decode* (byte -> `[0, 1]`) is `byte as f32 /
//! 255.0`, matching every conformant fixed-function texture fetch. That is
//! not interchangeable with a reciprocal-multiply decode (`byte as f32 *
//! (1.0 / 255.0)`): the two round differently for 126 of the 256 possible
//! byte values, which would make this reference disagree with a correct
//! shader wholesale rather than by the handful of FMA-reachable samples
//! recorded in spec §9.1.

/// Chroma is centred on 128/255, matching the forward shader's `+ 0.502`.
pub const CHROMA_CENTRE: f32 = 0.502;

/// Exact inverse of the forward matrix. The textbook full-range BT.601
/// inverse differs from this by at most 0.1587/255 across the whole YUV
/// cube -- exhaustively measured, at `(cb=0, cr=0)` on the R channel, not
/// assumed -- so the difference is invisible. These are used because
/// inverting the transform the encoder actually applied is the correct
/// thing to do.
pub const R_COEFF: [f32; 3] = [1.0, -0.000927, 1.401687];
pub const G_COEFF: [f32; 3] = [1.0, -0.343695, -0.714169];
// Kept at the same six-digit precision as `1.772160` in
// h264_nv12_blit.wgsl, deliberately, even though clippy would otherwise
// trim the trailing zero: the shader header says the two files must change
// together, and a side-by-side read is the only thing that catches drift
// between them. `clippy::excessive_precision` isn't wrong that `1.77216`
// round-trips through f32 identically -- it does -- but spelling it
// differently here than in the WGSL defeats that safety net for no
// benefit.
#[allow(clippy::excessive_precision)]
pub const B_COEFF: [f32; 3] = [1.0, 1.772160, 0.000990];

/// Convert one already-selected NV12 sample triple to RGBA8.
///
/// Pure matrix arithmetic: `luma`/`cb`/`cr` are raw 8-bit samples the caller
/// has already picked out of the two planes. This function does not know
/// chroma is stored at half resolution and performs no upsampling -- see
/// [`nv12_plane_pixel_to_rgba`] for the function that does, and the module
/// doc for why the two are kept separate.
pub fn nv12_pixel_to_rgba(luma: u8, cb: u8, cr: u8) -> [u8; 4] {
    let y = luma as f32 / 255.0;
    let u = cb as f32 / 255.0 - CHROMA_CENTRE;
    let v = cr as f32 / 255.0 - CHROMA_CENTRE;

    let r = R_COEFF[0] * y + R_COEFF[1] * u + R_COEFF[2] * v;
    let g = G_COEFF[0] * y + G_COEFF[1] * u + G_COEFF[2] * v;
    let b = B_COEFF[0] * y + B_COEFF[1] * u + B_COEFF[2] * v;

    [to_u8(r), to_u8(g), to_u8(b), 255]
}

/// Convert the pixel at `(x, y)` of a full NV12 surface to RGBA8, including
/// the nearest-neighbour chroma upsampling `h264_nv12_blit.wgsl` performs
/// with `p / 2`.
///
/// `luma`/`luma_stride` and `chroma`/`chroma_stride` are the two planes as a
/// decoder would hand them back: `luma` is one byte per sample, `chroma` is
/// two bytes per sample (Cb, Cr interleaved), both row-major with `stride`
/// bytes between rows (which may exceed the logical row width -- alignment
/// padding, exactly as `DmabufPlanes::luma`/`chroma` in
/// `ghostframe-client-h264` carry pitch separately from width for that
/// reason).
///
/// The chroma sample is selected at `(x / 2, y / 2)` -- integer division,
/// matching the forward shader's `if (local_idx == 0)` top-left-of-2x2-block
/// sampling and the WGSL's `p / 2`. A surface with odd width or height has a
/// chroma plane sized `width.div_ceil(2)` x `height.div_ceil(2)`, the same
/// `div_ceil` semantics as `DmabufPlanes::chroma_width()`/`chroma_height()`;
/// the caller's `chroma_stride` must already reflect that, not `width / 2`.
///
/// # Panics
/// If `(x, y)` is out of bounds for the strides given, i.e. the computed
/// plane offset would read past the end of `luma` or `chroma`.
pub fn nv12_plane_pixel_to_rgba(
    luma: &[u8],
    luma_stride: usize,
    chroma: &[u8],
    chroma_stride: usize,
    x: u32,
    y: u32,
) -> [u8; 4] {
    let luma_idx = y as usize * luma_stride + x as usize;
    let chroma_idx = (y / 2) as usize * chroma_stride + (x / 2) as usize * 2;

    let l = luma[luma_idx];
    let cb = chroma[chroma_idx];
    let cr = chroma[chroma_idx + 1];
    nv12_pixel_to_rgba(l, cb, cr)
}

/// The fixed-function unorm8 **write**: round the exact product `v * 255` to
/// the nearest integer, ties away from zero (Vulkan's `SFLOAT`/`UNORM`
/// conversion rule, and what a render target write does in hardware).
///
/// This is deliberately NOT `(v * 255.0 + 0.5) as u8` evaluated in `f32`,
/// which is the idiom `bgra_to_nv12.comp:52` uses -- and is correct there,
/// because that shader *defines* the forward quantisation: whatever it
/// computes, in whatever precision, IS the ground truth for what the
/// encoder wrote. This function is the opposite role: it *models* a
/// fixed-function write that real hardware is required to get exactly
/// right, so it must reproduce the exact-product rounding, not one more
/// f32-rounded step removed from it.
///
/// Doing the multiply in `f32` costs exactness: `v * 255.0` first rounds to
/// the nearest representable `f32`, and when the true product sits just
/// under a half-integer, that rounding can land it exactly ON the
/// half-integer -- after which `+ 0.5` carries it across to the wrong
/// integer. Measured case: `luma=119, cb=58, cr=147` on the G channel gives
/// an exact channel value of `0.50784314...`, whose exact product with 255
/// is `129.4999998807907`, which rounds to 129 under any correct reading --
/// but `f32(0.50784314 * 255.0)` is exactly `129.5`, so the f32-only path
/// computes `130`. Computing the product in `f64` keeps enough precision
/// that this can't happen for any `f32` input in `[0, 1]`.
fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) as f64 * 255.0 + 0.5).floor() as u8
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
    ///
    /// This reaches 255 by two mechanisms that happen to coincide: the
    /// forward hand-computation below clamps explicitly (mirroring
    /// `bgra_to_nv12.comp`'s own clamp), while `nv12_pixel_to_rgba`'s
    /// `to_u8` gets there via `f64`'s saturating `as u8` cast landing
    /// exactly at the boundary. The three constants (`0.299`, `-0.169`,
    /// `0.500`) are also hand-copied from the forward shader with nothing
    /// to catch drift if that shader's matrix ever changes -- a correctness
    /// test whose own fidelity to the thing it is testing rests on the
    /// reader, not the compiler.
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

    /// The exact worked case from the double-rounding review (C1(A)): an
    /// `f32`-only `v * 255.0 + 0.5` computes 130 for the G channel here;
    /// the true rounding of the exact product is 129. Pins the regression
    /// directly rather than only through the full-cube sweep, which is not
    /// part of this crate's test suite.
    #[test]
    fn to_u8_does_not_double_round_a_near_half_integer() {
        let rgba = nv12_pixel_to_rgba(119, 58, 147);
        assert_eq!(
            rgba[1], 129,
            "G channel should round the exact product (129.4999998...) down to 129, \
             not double-round through an f32 intermediate to 130"
        );
    }

    /// The plane-level function must select chroma at `(x/2, y/2)` -- not
    /// `(x, y)` and not a fixed offset -- and must respect a chroma stride
    /// computed with `div_ceil`, which is what an odd-dimensioned surface
    /// forces. A 5x3 luma plane needs a `3x2` chroma plane
    /// (`5.div_ceil(2) x 3.div_ceil(2)`), and every 2x2 luma block must read
    /// back the one chroma sample nearest-neighbour replication says it
    /// should.
    #[test]
    fn plane_pixel_selects_chroma_by_nearest_neighbour_with_div_ceil_stride() {
        const W: usize = 5;
        const H: usize = 3;
        let chroma_w = (W as u32).div_ceil(2) as usize; // 3
        let chroma_h = (H as u32).div_ceil(2) as usize; // 2

        // Luma: value at (x, y) is a unique byte so a wrong pixel read is
        // obvious rather than coincidentally right.
        let mut luma = vec![0u8; W * H];
        for y in 0..H {
            for x in 0..W {
                luma[y * W + x] = (y * W + x) as u8;
            }
        }
        // Chroma: two bytes per sample, (cb, cr) = (100 + index, 200 +
        // index), also unique per chroma texel.
        let mut chroma = vec![0u8; chroma_w * chroma_h * 2];
        for cy in 0..chroma_h {
            for cx in 0..chroma_w {
                let i = (cy * chroma_w + cx) as u8;
                chroma[(cy * chroma_w + cx) * 2] = 100 + i;
                chroma[(cy * chroma_w + cx) * 2 + 1] = 200 + i;
            }
        }

        for y in 0..H as u32 {
            for x in 0..W as u32 {
                let got = nv12_plane_pixel_to_rgba(&luma, W, &chroma, chroma_w * 2, x, y);
                let l = luma[(y as usize) * W + x as usize];
                let cx = (x / 2) as usize;
                let cy = (y / 2) as usize;
                let cb = chroma[(cy * chroma_w + cx) * 2];
                let cr = chroma[(cy * chroma_w + cx) * 2 + 1];
                assert_eq!(
                    got,
                    nv12_pixel_to_rgba(l, cb, cr),
                    "pixel ({x},{y}) should read luma={l} chroma=({cb},{cr})"
                );
            }
        }
    }
}
