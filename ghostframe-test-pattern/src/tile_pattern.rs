//! `--tile-pattern <class> --drm-direct` mode.
//!
//! Fills the full scanout framebuffer with a 32×32 `ContentClass` fixture tile
//! tiled to cover the entire screen. Each `ContentClass` variant maps to one of
//! the M3.5 Layer B "pure scenes" (solid, flat_ui, text, gradient, photo,
//! motion).
//!
//! Used by the Layer B `codec_report` binary to drive deterministic, class-pure
//! scenes through the codec pipeline without relying on a live X session.
//!
//! ## Pixel format
//!
//! Fixture tiles are BGRA8 — bytes `[B, G, R, A]`. The DRM scanout expects
//! XRGB8888 little-endian — bytes `[B, G, R, X]`. The B/G/R bytes are at the
//! same positions, so we just copy the fixture bytes and let the X/A byte be
//! whatever the fixture emits (the scanout's X channel is a don't-care).
//!
//! ## Motion
//!
//! `ContentClass::Motion::tile()` returns a fixed representative frame.  The
//! paint loop re-invokes `tile()` each frame tick so that if the fixture ever
//! becomes animated (e.g. time-seeded), the scanout will update automatically.
//! For the current static implementation this results in identical bytes every
//! tick — acceptable per M3.5 spec §Architecture ("pure-scene model").

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};

/// Fill `out` (a flat `frame_w × frame_h × 4` byte slice) with the 32×32 tile
/// from `class` repeated to cover every pixel.
///
/// `out` is the mmap slice for the DRM dumb buffer; `stride` may be larger than
/// `frame_w * 4` (hardware pitch padding). We write only the active pixel area
/// and leave any pitch padding untouched.
pub fn fill_with_tile_pattern(
    class_name: &str,
    frame_w: u32,
    frame_h: u32,
    pitch: usize,
    out: &mut [u8],
) {
    use ghostframe_bench::fixtures::ContentClass;
    let class = match class_name {
        "solid" => ContentClass::Solid,
        "flat_ui" => ContentClass::FlatUi,
        "text" => ContentClass::Text,
        "gradient" => ContentClass::Gradient,
        "photo" => ContentClass::Photo,
        "motion" => ContentClass::Motion,
        other => panic!("--tile-pattern: unknown class {other:?}"),
    };
    let tile = class.tile(); // 32×32 BGRA = 4096 bytes
    debug_assert_eq!(tile.len(), 4096);

    let tile_w = 32u32;
    let tile_h = 32u32;
    let row_active = (frame_w * 4) as usize;
    for fy in 0..frame_h as usize {
        let ty = fy % tile_h as usize;
        let row_src = &tile[ty * tile_w as usize * 4..(ty + 1) * tile_w as usize * 4];
        let row_dst = &mut out[fy * pitch..fy * pitch + row_active];
        for fx_tile in 0..(frame_w / tile_w) as usize {
            let off = fx_tile * tile_w as usize * 4;
            row_dst[off..off + tile_w as usize * 4].copy_from_slice(row_src);
        }
    }
}

/// Paint the tile-pattern scene once, then sleep forever.
///
/// For `motion` the fixture is static on the first paint; the scene is held on
/// scanout without repainting.  Layer B bench measurement runs instantaneously
/// against a pre-painted buffer, so motion animation is not required here.
pub fn run(card_path: &str, class_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = setup_dumb_scanout(card_path)?;
    let width = scanout.mode_w;
    let height = scanout.mode_h;
    let pitch = scanout.pitch as usize;
    eprintln!(
        "tile-pattern: scanout {}x{} active — painting class {:?}",
        width, height, class_name
    );

    {
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();

        fill_with_tile_pattern(class_name, width, height, pitch, bytes);
        msync_buffer(bytes);
    }

    eprintln!("tile-pattern: scene painted — holding scanout");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
