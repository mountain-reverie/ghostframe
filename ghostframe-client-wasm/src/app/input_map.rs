// SPDX-License-Identifier: MIT OR Apache-2.0
//! Browser input to the wire's terms: X11 button numbers and framebuffer
//! pixels. The encoding and the keysym table are `ghostframe-client-core::input`.

/// DOM `MouseEvent.button` to X11: DOM 0 left, 1 middle, 2 right, 3 back,
/// 4 forward; X11 1, 2, 3, 8, 9. Any other button is not sent.
pub fn dom_button_to_x11(button: i16) -> Option<u8> {
    match button {
        0 => Some(1),
        1 => Some(2),
        2 => Some(3),
        3 => Some(8),
        4 => Some(9),
        _ => None,
    }
}

/// Toward zero, then clamped to `i16` (NaN becomes 0, as it did crossing the
/// old JS boundary).
pub fn quantize(n: f64) -> i16 {
    n.trunc().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// `getBoundingClientRect()` of the canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasRect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// A pointer at (`client_x`, `client_y`) in framebuffer pixels; (0, 0) while
/// the canvas has no size or no frame has been announced.
pub fn canvas_to_fb(client_x: f64, client_y: f64, rect: CanvasRect, fb: (u32, u32)) -> (i16, i16) {
    if rect.width <= 0.0 || rect.height <= 0.0 || fb.0 == 0 || fb.1 == 0 {
        return (0, 0);
    }
    (
        quantize((client_x - rect.left) / rect.width * f64::from(fb.0)),
        quantize((client_y - rect.top) / rect.height * f64::from(fb.1)),
    )
}

/// One wheel tick per axis whatever the delta's unit; `None` when neither moved.
pub fn wheel_ticks(delta_x: f64, delta_y: f64) -> Option<(i16, i16)> {
    let sign = |d: f64| {
        if d > 0.0 {
            1
        } else if d < 0.0 {
            -1
        } else {
            0
        }
    };
    let (dx, dy) = (sign(delta_x), sign(delta_y));
    (dx != 0 || dy != 0).then_some((dx, dy))
}

/// The pointer-move the canvas sends when the pointer leaves it, so the
/// server can hide its cursor.
pub const POINTER_LEFT: (i16, i16) = (-1, -1);

#[cfg(test)]
mod tests {
    use super::*;

    const RECT: CanvasRect = CanvasRect {
        left: 10.0,
        top: 20.0,
        width: 960.0,
        height: 540.0,
    };

    #[test]
    fn buttons_map_to_x11_and_others_are_dropped() {
        let got: Vec<_> = (0..6).map(dom_button_to_x11).collect();
        assert_eq!(got, [Some(1), Some(2), Some(3), Some(8), Some(9), None]);
        assert_eq!(dom_button_to_x11(-1), None);
    }

    #[test]
    fn the_canvas_scales_to_the_framebuffer() {
        // A 1920x1080 framebuffer drawn at half size, offset (10, 20).
        assert_eq!(canvas_to_fb(10.0, 20.0, RECT, (1920, 1080)), (0, 0));
        assert_eq!(canvas_to_fb(490.0, 290.0, RECT, (1920, 1080)), (960, 540));
        assert_eq!(
            canvas_to_fb(10.9, 20.9, RECT, (1920, 1080)),
            (1, 1),
            "toward zero"
        );
    }

    #[test]
    fn no_size_means_the_origin() {
        assert_eq!(canvas_to_fb(500.0, 300.0, RECT, (0, 0)), (0, 0));
        let flat = CanvasRect { width: 0.0, ..RECT };
        assert_eq!(canvas_to_fb(500.0, 300.0, flat, (1920, 1080)), (0, 0));
    }

    #[test]
    fn quantize_clamps_and_maps_nan_to_zero() {
        assert_eq!(quantize(-1.7), -1);
        assert_eq!(quantize(1e9), i16::MAX);
        assert_eq!(quantize(-1e9), i16::MIN);
        assert_eq!(quantize(f64::NAN), 0);
    }

    #[test]
    fn the_wheel_sends_signs_and_nothing_for_no_motion() {
        assert_eq!(wheel_ticks(0.0, 120.0), Some((0, 1)));
        assert_eq!(wheel_ticks(-3.0, -0.5), Some((-1, -1)));
        assert_eq!(wheel_ticks(0.0, -0.0), None);
    }
}
