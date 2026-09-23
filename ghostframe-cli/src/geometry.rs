//! Fullscreen centring and pointer mapping.
//!
//! The window starts fullscreen; the remote image is drawn 1:1 and centred,
//! with the surplus black. Scaling is an explicit non-goal, so the mapping
//! from window coordinates to remote framebuffer coordinates is a pure
//! offset (plus clamping to the image bounds).

/// Where the remote image sits inside the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub origin_x: i32,
    pub origin_y: i32,
    pub image_w: u32,
    pub image_h: u32,
    pub out_w: u32,
    pub out_h: u32,
}

impl Placement {
    /// Centre `image` inside `out`. Never yields a negative origin: an image
    /// larger than the output is pinned to 0 and cropped, because a negative
    /// origin would be subtracted into nonsense by `map_pointer`.
    pub fn centre(image_w: u32, image_h: u32, out_w: u32, out_h: u32) -> Self {
        let origin_x = centre_offset(image_w, out_w);
        let origin_y = centre_offset(image_h, out_h);
        Self {
            origin_x,
            origin_y,
            image_w,
            image_h,
            out_w,
            out_h,
        }
    }
}

/// Half the surplus of `out` over `image` along one axis, or 0 if `image` is
/// as large as or larger than `out` on that axis (nothing to centre; pin to
/// the top/left edge so the crop is well-defined).
fn centre_offset(image: u32, out: u32) -> i32 {
    if out > image {
        ((out - image) / 2) as i32
    } else {
        0
    }
}

/// Window coordinates -> remote framebuffer coordinates, clamped to the
/// image. The wire carries `i16`, so the conversion saturates rather than
/// wrapping: a pathological remote width must not turn a large clamped
/// coordinate into a negative one.
pub fn map_pointer(p: &Placement, win_x: i32, win_y: i32) -> (i16, i16) {
    let image_x = win_x - p.origin_x;
    let image_y = win_y - p.origin_y;

    let max_x = p.image_w.saturating_sub(1) as i32;
    let max_y = p.image_h.saturating_sub(1) as i32;

    let clamped_x = image_x.clamp(0, max_x);
    let clamped_y = image_y.clamp(0, max_y);

    (saturate_to_i16(clamped_x), saturate_to_i16(clamped_y))
}

fn saturate_to_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}
