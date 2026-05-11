//! `--drm-direct` + `--mode-switch-cycle SECS` mode.
//!
//! Becomes DRM master on `/dev/dri/card<N>`, allocates a dumb buffer matching
//! the first connected output's preferred mode, attaches it as the CRTC's
//! scanout framebuffer, then loops painting alternating static/motion content
//! directly into the buffer's mmap.
//!
//! This bypasses Xorg entirely. It's the e2e-friendly alternative to the
//! X11-based `mode_switch.rs` for environments where Xorg holds the DRM
//! master and prevents framebuffer-update propagation (e.g. VKMS).
//!
//! Pixel format: XRGB8888 / DRM_FOURCC_XR24, packed as `[B, G, R, X]` little
//! endian. Same convention used by VKMS' writeback target buffer.
//!
//! Resource cleanup: this is a test fixture that runs an infinite paint
//! loop. The framebuffer + dumb buffer + DRM master are released by the
//! kernel on process exit (including SIGKILL). For long-lived test
//! sessions that get repeatedly SIGKILLed, the kernel may accumulate
//! GEM handles in VKMS driver state — restart vkms via `rmmod && modprobe`
//! if you see DRM-side resource exhaustion.

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::thread;
use std::time::{Duration, Instant};

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{connector, crtc, Device as ControlDevice, Mode};
use drm::Device;

/// Background colour for the static half — packed XRGB8888 little-endian, so
/// in memory bytes are `[B=0xA0, G=0x40, R=0x20, X=0x00]`. Matches the
/// X11-based mode_switch's intent (steel-blue-ish).
const STATIC_BG_PIXEL: u32 = 0x00_20_40_A0;
const MOTION_TICK: Duration = Duration::from_millis(50);
/// Band height matches `ghostframe-lib::tile::TILE_SIZE` (32 px) so every
/// motion-band exactly covers one tile row in the capture grid. Kept in sync
/// manually since `ghostframe-test-pattern` has no `ghostframe-lib`
/// dependency.
const BAND_H: u32 = 32;

/// Thin wrapper around an open DRM device file so we can implement the
/// `drm` crate's traits.
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

/// Become the DRM master, set up a dumb-buffer scanout, and run the
/// static/motion paint loop.
pub fn run(card_path: &str, half_cycle: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(card_path)
        .map_err(|e| format!("open {card_path}: {e}"))?;
    let card = Card(file);

    // Try to acquire DRM master. If the kernel says we're already the
    // master (privileged container with no other client), this returns Ok.
    // If another client (Xorg) holds it, we fail loudly so the test
    // operator knows to remove that client.
    if let Err(e) = drm_ffi::auth::acquire_master(card.as_fd()) {
        // EACCES typically means another DRM client (Xorg/Wayland) owns
        // master and `set_crtc` will also EACCES — surface that clearly.
        // Other errors (e.g. EINVAL on some kernels when we already are
        // master implicitly) are noisier and not necessarily fatal; we log
        // and continue — modeset below will tell us authoritatively.
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            eprintln!(
                "drm_direct: acquire_master EACCES — another DRM client owns master \
                 on {card_path}. If set_crtc below also EACCES, check that no \
                 Xorg/Wayland compositor is holding this device (host config: \
                 /etc/X11/xorg.conf.d/*-ignore-vkms.conf with MatchDriver+Ignore=true)."
            );
        } else {
            eprintln!(
                "drm_direct: acquire_master returned {e} (continuing; may already be master)"
            );
        }
    } else {
        eprintln!("drm_direct: acquired DRM master on {card_path}");
    }

    let res = card.resource_handles()?;

    // Find the first connected connector that has at least one mode.
    let mut chosen: Option<(connector::Info, Mode)> = None;
    for ch in res.connectors() {
        let info = match card.get_connector(*ch, true) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if info.state() != connector::State::Connected {
            continue;
        }
        if let Some(m) = info.modes().first().copied() {
            chosen = Some((info, m));
            break;
        }
    }
    let (conn_info, mode) = chosen
        .ok_or_else(|| "no connected DRM connector with a valid mode".to_string())?;

    let (mode_w, mode_h) = mode.size();
    let mode_w = mode_w as u32;
    let mode_h = mode_h as u32;
    eprintln!(
        "drm_direct: connector {:?} ({}), mode {}x{}",
        conn_info.interface(),
        conn_info.interface_id(),
        mode_w,
        mode_h
    );

    // Pick a CRTC that's compatible with one of the connector's encoders.
    let mut chosen_crtc: Option<crtc::Handle> = None;
    'outer: for enc_h in conn_info.encoders() {
        let enc_info = match card.get_encoder(*enc_h) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let possible = res.filter_crtcs(enc_info.possible_crtcs());
        for c in possible {
            chosen_crtc = Some(c);
            break 'outer;
        }
    }
    let crtc_handle = chosen_crtc.ok_or_else(|| {
        "no compatible CRTC found for connector's encoders".to_string()
    })?;
    eprintln!("drm_direct: CRTC handle = {:?}", crtc_handle);

    // Create the dumb buffer that will be the scanout source.
    let mut db = card
        .create_dumb_buffer((mode_w, mode_h), DrmFourcc::Xrgb8888, 32)
        .map_err(|e| format!("create_dumb_buffer({mode_w}x{mode_h}): {e}"))?;
    let pitch = db.pitch();
    eprintln!("drm_direct: dumb buffer pitch = {} bytes", pitch);

    // Wrap as a framebuffer (depth 24, bpp 32 — XRGB8888).
    let fb = card
        .add_framebuffer(&db, 24, 32)
        .map_err(|e| format!("add_framebuffer: {e}"))?;

    // Pre-fill with the static background so set_crtc has valid content.
    fill_solid(&card, &mut db, pitch, mode_w, mode_h, STATIC_BG_PIXEL)?;

    // Modeset: bind FB to CRTC and connector. This is where master-conflict
    // EACCES would surface.
    card.set_crtc(
        crtc_handle,
        Some(fb),
        (0, 0),
        &[conn_info.handle()],
        Some(mode),
    )
    .map_err(|e| format!("set_crtc: {e}"))?;
    eprintln!("drm_direct: scanout active — entering paint loop");

    let cycle = half_cycle * 2;
    let mut motion_phase: u8 = 0;

    loop {
        let cycle_start = Instant::now();

        // Static half: paint the buffer once with the solid colour.
        fill_solid(&card, &mut db, pitch, mode_w, mode_h, STATIC_BG_PIXEL)?;
        thread::sleep(half_cycle);

        // Motion half: rewrite tile-aligned bands of shifting colours every
        // MOTION_TICK so the capture grid sees fresh dirty tiles every frame.
        let motion_deadline = cycle_start + cycle;
        while Instant::now() < motion_deadline {
            paint_motion(&card, &mut db, pitch, mode_w, mode_h, motion_phase)?;
            motion_phase = motion_phase.wrapping_add(7);
            thread::sleep(MOTION_TICK);
        }
    }
}

/// Fill the buffer with a single packed-XRGB pixel value. Writes only the
/// active `width * 4` bytes per row; any trailing pitch padding is left
/// untouched (it isn't sampled by VKMS, but being conservative avoids
/// surprises if this code is ever reused on a real GPU with a tighter
/// pitch contract).
fn fill_solid(
    card: &Card,
    db: &mut drm::control::dumbbuffer::DumbBuffer,
    pitch: u32,
    width: u32,
    height: u32,
    pixel: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut map = card
        .map_dumb_buffer(db)
        .map_err(|e| format!("map_dumb_buffer: {e}"))?;
    let bytes = map.as_mut();
    let row_pitch = pitch as usize;
    let row_active = (width as usize) * 4;
    let pixel_bytes = pixel.to_le_bytes();
    for y in 0..height as usize {
        let row_start = y * row_pitch;
        let row_end = row_start + row_active;
        let row = &mut bytes[row_start..row_end];
        for chunk in row.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel_bytes);
        }
    }
    msync_buffer(bytes);
    Ok(())
}

/// Force CPU caches and dirty pages to be synchronously flushed back to
/// the kernel-managed shmem pages backing the dumb buffer. This is the
/// best-effort coherency hammer for VKMS, where the dumb buffer's pages
/// are also exposed (read-side) to a Vulkan importer via PRIME-exported
/// DMA-BUF. Without this, GPU reads can lag by a frame or two and the
/// SAD-based dirty detector sees zero changes.
fn msync_buffer(bytes: &mut [u8]) {
    // SAFETY: `bytes` came from a successful `map_dumb_buffer` mmap, so
    // (ptr, len) is a valid mapped region. MS_SYNC blocks until writes are
    // committed; MS_INVALIDATE drops other-mapping cache lines.
    unsafe {
        let _ = libc::msync(
            bytes.as_mut_ptr() as *mut libc::c_void,
            bytes.len(),
            libc::MS_SYNC | libc::MS_INVALIDATE,
        );
    }
}

/// Paint horizontal bands of shifting colours, each `BAND_H` rows tall.
fn paint_motion(
    card: &Card,
    db: &mut drm::control::dumbbuffer::DumbBuffer,
    pitch: u32,
    width: u32,
    height: u32,
    motion_phase: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut map = card
        .map_dumb_buffer(db)
        .map_err(|e| format!("map_dumb_buffer: {e}"))?;
    let bytes = map.as_mut();
    let row_pitch = pitch as usize;
    let row_active = (width as usize) * 4;
    let mut band_idx: u8 = 0;
    let mut y = 0u32;
    while y < height {
        let r = motion_phase.wrapping_mul(3).wrapping_add(band_idx);
        let g = motion_phase
            .wrapping_mul(5)
            .wrapping_add(band_idx ^ 0x55);
        let b = motion_phase
            .wrapping_mul(7)
            .wrapping_add(band_idx ^ 0xAA);
        // XRGB8888 little-endian: bytes [B, G, R, X]
        let pixel = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        let pixel_bytes = pixel.to_le_bytes();

        let band_end = (y + BAND_H).min(height);
        for row in y..band_end {
            let row_start = (row as usize) * row_pitch;
            let row_slice = &mut bytes[row_start..row_start + row_active];
            for chunk in row_slice.chunks_exact_mut(4) {
                chunk.copy_from_slice(&pixel_bytes);
            }
        }

        y += BAND_H;
        band_idx = band_idx.wrapping_add(1);
    }
    msync_buffer(bytes);
    Ok(())
}
