//! `--drm-direct --scroll-text`: a text document scrolling vertically at a
//! fixed rate, then held still.
//!
//! This is the stimulus for the text-under-motion bench scene (scroll
//! detection design note, "Bench scene"; protocol comparative review 4.4 and
//! 5.4). The document is *procedural*: pixel row `r` of the document is a pure
//! function of `r`, so the frame at scroll offset `o` is
//! [`render_scroll_frame`]`(w, h, o)`, and the harness renders exactly the
//! same bytes offline to use as an SSIM reference. The paint loop and the
//! harness share this one function; there is no second copy of the layout.
//!
//! ## The document
//!
//! Lines of pseudo-words built from the letters the bundled 6x8 bitmap font
//! actually has (`AEFGHILMORST`), drawn at 2x (12x16 cells, 24 px line pitch,
//! roughly 16 px body text), light ink on a dark ground, no anti-aliasing,
//! with a blank line between paragraphs. It is not English, and it is
//! two-colour where real desktop text is anti-aliased. Both matter for
//! absolute numbers (a real font gives the classifier more colours per tile);
//! neither changes the structural point being measured -- a screenful of
//! dense high-contrast edges all of which move by a few rows per frame.
//!
//! ## Timeline and signalling
//!
//! 1. Paint offset 0 and hold. If `go_file` is given, wait for that file to
//!    appear (the harness creates it with `docker exec ... touch` once the
//!    browser is connected and has converged); otherwise start after 1 s.
//! 2. Emit `scroll-event phase=start mono_ns=<CLOCK_MONOTONIC ns>` on stderr,
//!    then paint `scroll_secs * fps` further frames, `rows_per_frame` rows
//!    lower each, on a fixed deadline schedule.
//! 3. Emit `scroll-event phase=stop mono_ns=... final_offset_rows=...` and hold
//!    the final frame forever (a keep-alive `msync` only, no pixel writes).
//!
//! `CLOCK_MONOTONIC` is the same clock the server's `ghostframe::bench`
//! telemetry stamps (`capture_done_ns`), and containers share the host's, so
//! the harness can place the scroll window on the telemetry timeline with no
//! clock-offset estimation.
//!
//! `rows_per_frame == 0` is the control: identical timeline and events, but
//! the screen never changes.

use std::thread;
use std::time::{Duration, Instant};

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};
use crate::font::{pixel_set, GLYPH_H, GLYPH_W};
use crate::text_grid::{BG_PIXEL, FG_PIXEL};

/// Integer upscale applied to the 6x8 font.
pub const SCALE: u32 = 2;
/// Document pixel rows per text line.
pub const LINE_PITCH: u32 = 24;
/// Rows of padding above the glyph inside a line cell.
const GLYPH_TOP: u32 = 4;
/// Left/right margin in pixels.
pub const MARGIN_X: u32 = 16;
/// Letters the pseudo-words are built from. All have real glyphs (anything
/// else would render as the font's solid "missing glyph" block).
pub const ALPHABET: &[u8] = b"AEFGHILMORST";

/// Rows per frame / duration / rate defaults of the bench scene.
pub const DEFAULT_ROWS_PER_FRAME: u32 = 3;
pub const DEFAULT_SCROLL_SECS: u64 = 10;
pub const DEFAULT_FPS: u32 = 30;
/// Where the paint loop looks for the go-signal by default.
pub const DEFAULT_GO_FILE: &str = "/tmp/ghostframe-scroll-go";

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Characters that fit on one line at `width` pixels.
pub fn chars_per_line(width: u32) -> usize {
    (width.saturating_sub(2 * MARGIN_X) / (GLYPH_W * SCALE)) as usize
}

/// The text of document line `line` (deterministic, <= `max_chars`).
/// Every ninth line is blank (paragraph break).
pub fn line_text(line: u64, max_chars: usize) -> String {
    if line % 9 == 8 {
        return String::new();
    }
    let mut state = splitmix(line ^ 0xD0C0_FFEE);
    let mut next = || {
        state = splitmix(state);
        state
    };
    // Ragged right edge: between 55% and 100% of the line.
    let target = max_chars * (55 + (next() % 46) as usize) / 100;
    let mut out = String::new();
    while out.len() < target {
        let wlen = 2 + (next() % 7) as usize;
        if out.len() + wlen + 1 > max_chars {
            break;
        }
        for _ in 0..wlen {
            out.push(ALPHABET[(next() % ALPHABET.len() as u64) as usize] as char);
        }
        out.push(' ');
    }
    out.trim_end().to_string()
}

/// Render the frame showing document rows `offset_rows..offset_rows+height`
/// into `buf` (BGRX, little-endian XRGB8888 -- the scanout format), rows
/// `stride` bytes apart. Rows are written for `0..height`, `width*4` bytes
/// each.
pub fn render_scroll_frame_into(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    offset_rows: u64,
) {
    let bg = BG_PIXEL.to_le_bytes();
    let fg = FG_PIXEL.to_le_bytes();
    let max_chars = chars_per_line(width);
    let mut cached_line = u64::MAX;
    let mut cached_text: Vec<u8> = Vec::new();
    for y in 0..height as usize {
        let doc_row = offset_rows + y as u64;
        let line = doc_row / LINE_PITCH as u64;
        let ry = (doc_row % LINE_PITCH as u64) as u32;
        let row = &mut buf[y * stride..y * stride + width as usize * 4];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&bg);
        }
        if !(GLYPH_TOP..GLYPH_TOP + GLYPH_H * SCALE).contains(&ry) {
            continue;
        }
        if line != cached_line {
            cached_line = line;
            cached_text = line_text(line, max_chars).into_bytes();
        }
        let gy = (ry - GLYPH_TOP) / SCALE;
        for (n, &ch) in cached_text.iter().enumerate() {
            if ch == b' ' {
                continue;
            }
            for gx in 0..GLYPH_W {
                if pixel_set(ch as char, gx, gy) {
                    let x0 = (MARGIN_X + (n as u32 * GLYPH_W + gx) * SCALE) as usize;
                    for dx in 0..SCALE as usize {
                        let o = (x0 + dx) * 4;
                        if o + 4 <= row.len() {
                            row[o..o + 4].copy_from_slice(&fg);
                        }
                    }
                }
            }
        }
    }
}

/// Tightly packed BGRX frame (`width*4` bytes per row) at `offset_rows`.
pub fn render_scroll_frame(width: u32, height: u32, offset_rows: u64) -> Vec<u8> {
    let mut v = vec![0u8; width as usize * height as usize * 4];
    render_scroll_frame_into(&mut v, width, height, width as usize * 4, offset_rows);
    v
}

/// `CLOCK_MONOTONIC` in nanoseconds (what the server's bench telemetry uses).
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid local; clock_gettime only writes through it.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub struct ScrollConfig {
    pub rows_per_frame: u32,
    pub scroll_secs: u64,
    pub fps: u32,
    /// Start when this path exists; `None` = start 1 s after the first paint.
    pub go_file: Option<String>,
}

fn present(
    scanout: &mut crate::drm_direct::DrmScanout,
    frame: Option<&[u8]>,
) -> Result<(), Box<dyn std::error::Error>> {
    let pitch = scanout.pitch as usize;
    let row_bytes = scanout.mode_w as usize * 4;
    let height = scanout.mode_h as usize;
    let mut map = scanout
        .card
        .map_dumb_buffer(&mut scanout.db)
        .map_err(|e| format!("map_dumb_buffer: {e}"))?;
    let bytes = map.as_mut();
    if let Some(f) = frame {
        // One bulk copy per row from a fully rendered local frame, so a
        // capture never sees a half-composed document.
        for y in 0..height {
            bytes[y * pitch..y * pitch + row_bytes]
                .copy_from_slice(&f[y * row_bytes..(y + 1) * row_bytes]);
        }
    }
    msync_buffer(bytes);
    Ok(())
}

pub fn run(card_path: &str, cfg: &ScrollConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = setup_dumb_scanout(card_path)?;
    let (w, h) = (scanout.mode_w, scanout.mode_h);
    eprintln!(
        "scroll_text: scanout {w}x{h} active rows_per_frame={} secs={} fps={}",
        cfg.rows_per_frame, cfg.scroll_secs, cfg.fps
    );
    let frame0 = render_scroll_frame(w, h, 0);
    present(&mut scanout, Some(&frame0))?;
    eprintln!("scroll_text: initial paint complete (offset 0)");

    let keepalive = Duration::from_millis(33);
    match &cfg.go_file {
        Some(p) => {
            while !std::path::Path::new(p).exists() {
                thread::sleep(keepalive);
                present(&mut scanout, None)?;
            }
        }
        None => thread::sleep(Duration::from_secs(1)),
    }

    let period = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    let frames = cfg.scroll_secs * cfg.fps as u64;
    let t0 = Instant::now();
    let start_ns = monotonic_ns();
    eprintln!("scroll-event phase=start mono_ns={start_ns}");
    let mut late = 0u64;
    let mut max_late = Duration::ZERO;
    for k in 1..=frames {
        let deadline = t0 + period * k as u32;
        let now = Instant::now();
        if now < deadline {
            thread::sleep(deadline - now);
        }
        let overshoot = Instant::now().saturating_duration_since(deadline);
        if overshoot > period / 2 {
            late += 1;
        }
        max_late = max_late.max(overshoot);
        if cfg.rows_per_frame > 0 {
            let frame = render_scroll_frame(w, h, k * cfg.rows_per_frame as u64);
            present(&mut scanout, Some(&frame))?;
        }
    }
    let final_offset = frames * cfg.rows_per_frame as u64;
    let stop_ns = monotonic_ns();
    eprintln!(
        "scroll-event phase=stop mono_ns={stop_ns} final_offset_rows={final_offset} \
         frames={frames} late_frames={late} max_late_ms={:.1}",
        max_late.as_secs_f64() * 1e3
    );
    loop {
        thread::sleep(keepalive);
        present(&mut scanout, None)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1920;
    const H: u32 = 1080;

    fn row(f: &[u8], y: u32) -> &[u8] {
        &f[y as usize * W as usize * 4..(y as usize + 1) * W as usize * 4]
    }

    #[test]
    fn render_is_deterministic() {
        assert_eq!(
            render_scroll_frame(W, H, 1234),
            render_scroll_frame(W, H, 1234)
        );
    }

    #[test]
    fn consecutive_offsets_differ_by_exactly_the_shift() {
        let mut exposed_ink = false;
        for base in [0u64, 7, 3000, 3001, 3002, 3003, 3004] {
            let a = render_scroll_frame(W, H, base);
            let b = render_scroll_frame(W, H, base + 3);
            assert_ne!(a, b, "a 3-row scroll must change the frame");
            // Every row of b except the 3 exposed at the bottom is a row of a
            // moved up by 3.
            for y in 0..H - 3 {
                assert_eq!(row(&b, y), row(&a, y + 3), "base {base} row {y}");
            }
            // The exposed strip is the next 3 document rows (checked against
            // a taller render), not a copy of anything already on screen.
            let tall = render_scroll_frame(W, H + 3, base);
            for y in H - 3..H {
                assert_eq!(row(&b, y), row(&tall, y + 3), "exposed row {y}");
            }
            let bg = BG_PIXEL.to_le_bytes();
            exposed_ink |= (H - 3..H).any(|y| row(&b, y).chunks_exact(4).any(|p| p != bg));
        }
        assert!(
            exposed_ink,
            "no base offset exposed any ink: test is vacuous"
        );
    }

    #[test]
    fn pitch_does_not_change_pixels() {
        let tight = render_scroll_frame(W, H, 99);
        let stride = W as usize * 4 + 64;
        let mut padded = vec![0xEEu8; stride * H as usize];
        render_scroll_frame_into(&mut padded, W, H, stride, 99);
        for y in 0..H as usize {
            assert_eq!(
                &padded[y * stride..y * stride + W as usize * 4],
                &tight[y * W as usize * 4..(y + 1) * W as usize * 4]
            );
        }
    }

    #[test]
    fn only_alphabet_letters_are_used() {
        for line in 0..400 {
            for c in line_text(line, chars_per_line(W)).chars() {
                assert!(c == ' ' || ALPHABET.contains(&(c as u8)), "{c:?}");
            }
        }
    }

    #[test]
    fn it_is_text_like() {
        // Dense, high-contrast, two-colour content: what the classifier is
        // meant to see as text.
        let f = render_scroll_frame(W, H, 5000);
        let px = |x: u32, y: u32| &row(&f, y)[x as usize * 4..x as usize * 4 + 4];
        let (mut ink, mut edges, mut pairs) = (0u64, 0u64, 0u64);
        for y in 0..H {
            for x in 0..W - 1 {
                if px(x, y) == FG_PIXEL.to_le_bytes() {
                    ink += 1;
                }
                pairs += 1;
                if px(x, y) != px(x + 1, y) {
                    edges += 1;
                }
            }
        }
        let ink_frac = ink as f64 / pairs as f64;
        let edge_frac = edges as f64 / pairs as f64;
        assert!(
            (0.04..0.40).contains(&ink_frac),
            "ink fraction {ink_frac:.3}"
        );
        assert!(edge_frac > 0.03, "horizontal edge density {edge_frac:.3}");
        // Per 32x32 tile: how many hold both colours (a PalRle-class tile)?
        let (mut two, mut tiles) = (0, 0);
        for ty in 0..H / 32 {
            for tx in 0..W / 32 {
                tiles += 1;
                let (mut has_bg, mut has_fg) = (false, false);
                for y in 0..32 {
                    for x in 0..32 {
                        if px(tx * 32 + x, ty * 32 + y) == FG_PIXEL.to_le_bytes() {
                            has_fg = true;
                        } else {
                            has_bg = true;
                        }
                    }
                }
                if has_bg && has_fg {
                    two += 1;
                }
            }
        }
        assert!(
            two as f64 / tiles as f64 > 0.6,
            "only {two}/{tiles} tiles contain ink"
        );
    }

    #[test]
    fn lines_are_ragged_and_differ() {
        let a = line_text(1, 150);
        let b = line_text(2, 150);
        assert!(!a.is_empty() && !b.is_empty());
        assert_ne!(a, b);
        assert!(line_text(8, 150).is_empty(), "paragraph break");
    }
}
