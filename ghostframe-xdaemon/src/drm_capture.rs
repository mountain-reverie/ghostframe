//! DRM framebuffer capture via PRIME DMA-BUF export.
//!
//! Two capture paths:
//!
//! 1. **Writeback connector path** (preferred when available, e.g. VKMS in
//!    e2e tests, or modern Intel/AMD/Rockchip iGPUs that expose writeback).
//!    On startup we probe the DRM device for a connector with
//!    [`drm::control::connector::Interface::Writeback`].  If found, we
//!    allocate a *persistent* dumb buffer sized to the active CRTC's mode
//!    and wrap it as a framebuffer (the "target FB").  Each capture call
//!    submits an atomic commit that attaches the writeback connector to
//!    the active CRTC with `WRITEBACK_FB_ID` = our target FB and
//!    `WRITEBACK_OUT_FENCE_PTR` = a pointer the kernel populates with a
//!    `sync_file` fd.  We `poll(POLLIN)` on that fd, then PRIME-export the
//!    target buffer's GEM handle.  The exported buffer contains a true
//!    snapshot of whatever was being scanned out.
//!
//! 2. **Modesetting framebuffer path** (fallback for hardware without a
//!    writeback connector).  Find the active CRTC, take the framebuffer
//!    that's currently attached, and PRIME-export its backing buffer
//!    directly.  Works on real hardware where the scanout BO is the FB
//!    Xorg writes to, but unreliable on VKMS where Xorg's post-startup
//!    writes don't propagate to the dumb buffer.
//!
//! `capture_prime_fd()` keeps a thread-safe internal cache so the
//! per-process probe and target-buffer allocation only happen once.
//!
//! ## DRM master conflicts
//!
//! Atomic commits to the writeback connector require the caller to be DRM
//! master (or hold a DRM lease covering the writeback connector). When
//! Xorg is running on the same card it holds master, so our atomic commit
//! is rejected with `EACCES`. We detect this at probe time via a
//! `TEST_ONLY` commit and gracefully fall back to the modesetting-FB
//! path.
//!
//! Long-term remedies (deferred):
//!
//! * **DRM lease via X RandR 1.6** — only works if Xorg's modesetting
//!   driver advertises the writeback connector as a RandR output. As of
//!   xorg-server 21.1, it does not; the writeback connector is invisible
//!   to RandR and therefore cannot be leased through the X server.
//! * **Direct kernel `drmModeCreateLease`** — must be called by the
//!   master, so requires a cooperating compositor.
//! * **Run xdaemon without Xorg** — xdaemon becomes master, sets its own
//!   modeset on the Virtual connector, and applications render directly
//!   into a buffer xdaemon owns. Architecturally invasive.
//!
//! On real hardware where xdaemon is the only DRM client (e.g. headless
//! servers without an X server), the writeback path works as designed.

use std::fs::File;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::{Mutex, OnceLock};

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{
    atomic::AtomicModeReq, connector, crtc, dumbbuffer::DumbBuffer, framebuffer, property,
    AtomicCommitFlags, Device as ControlDevice,
};
use drm::{ClientCapability, Device};

/// Thin wrapper around an open DRM device file.
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

// Both trait impls are empty: all methods are provided via AsFd.
impl Device for Card {}
impl ControlDevice for Card {}

/// Geometry of the captured framebuffer.
#[derive(Debug, Clone, Copy)]
pub struct FbGeometry {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Per-process state cached across captures: holds the open card, the
/// active CRTC, and (optionally) the writeback target buffer + property
/// handles needed to drive atomic commits.
struct CaptureState {
    card: Card,
    crtc: crtc::Handle,
    /// `Some(_)` if writeback is available; `None` for the fallback path.
    writeback: Option<WritebackState>,
}

struct WritebackState {
    connector: connector::Handle,
    crtc_id_prop: property::Handle,
    fb_id_prop: property::Handle,
    out_fence_ptr_prop: property::Handle,
    target_buffer: DumbBuffer,
    target_fb: framebuffer::Handle,
    width: u32,
    height: u32,
    /// Pitch of the target dumb buffer.
    stride: u32,
}

static STATE: OnceLock<Mutex<CaptureState>> = OnceLock::new();

/// Open the first available DRM card and prepare a capture path.
///
/// Returns a snapshot of the active framebuffer as a PRIME DMA-BUF fd
/// together with its geometry. Caller owns the fd.
///
/// First call performs the probe (open card, look for writeback connector,
/// allocate target buffer if applicable). Subsequent calls reuse the
/// cached state.
pub fn capture_prime_fd() -> std::io::Result<(OwnedFd, FbGeometry)> {
    let state_mutex = STATE.get_or_init(|| {
        Mutex::new(initialize_state().unwrap_or_else(|e| {
            // We can't return an error from get_or_init's closure; if
            // initialization fails, panic so the daemon's main() catches
            // it on next call. In practice initialize_state only fails if
            // /dev/dri is missing, which is fatal anyway.
            panic!("DRM capture state init failed: {e}");
        }))
    });
    let state = state_mutex.lock().expect("DRM capture state poisoned");

    // Borrow the writeback option separately so the rest of the state
    // (card, crtc) can be passed to the helper.
    if state.writeback.is_some() {
        // Re-borrow with an immutable view of the parts we need.
        let crtc = state.crtc;
        // We need a mutable borrow of writeback inside the helper for
        // map_dumb_buffer (which is called only on the dumb buffer; we
        // don't actually mutate the field). Pass split refs.
        let card = &state.card;
        let wb = state.writeback.as_ref().unwrap();
        return capture_via_writeback(card, crtc, wb);
    }

    // Fallback: legacy modesetting framebuffer path.
    capture_via_modesetting_fb(&state.card, state.crtc)
}

fn initialize_state() -> std::io::Result<CaptureState> {
    let card = open_first_card()?;
    let res = card.resource_handles()?;

    // Find an active CRTC (mode set + framebuffer attached).
    let mut active_crtc = None;
    let mut active_size = None;
    for c in res.crtcs() {
        let info = match card.get_crtc(*c) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if let (Some(mode), Some(_fb)) = (info.mode(), info.framebuffer()) {
            let (w, h) = mode.size();
            active_crtc = Some(*c);
            active_size = Some((w as u32, h as u32));
            break;
        }
    }
    let crtc = active_crtc
        .ok_or_else(|| std::io::Error::other("no active DRM CRTC with mode+framebuffer"))?;
    let (mode_w, mode_h) = active_size.unwrap();

    // Try to enable atomic + writeback caps; if either fails we silently
    // fall through to the legacy path.
    let writeback = match probe_writeback(&card, &res, crtc, mode_w, mode_h) {
        Ok(Some(wb)) => {
            tracing::info!(
                width = wb.width,
                height = wb.height,
                stride = wb.stride,
                "DRM writeback connector available — using writeback capture path"
            );
            Some(wb)
        }
        Ok(None) => {
            tracing::info!("no DRM writeback connector — using modesetting FB capture path");
            None
        }
        Err(e) => {
            tracing::warn!(
                "writeback probe failed ({e}); falling back to modesetting FB capture"
            );
            None
        }
    };

    Ok(CaptureState {
        card,
        crtc,
        writeback,
    })
}

/// Look for a writeback connector and, if present, set up a persistent
/// target buffer. Returns `Ok(None)` cleanly if no writeback connector is
/// present (this is the common case on real hardware that just has
/// HDMI/DP outputs); errors from the kernel during cap-set or buffer
/// allocation propagate as `Err(_)`.
fn probe_writeback(
    card: &Card,
    res: &drm::control::ResourceHandles,
    crtc: crtc::Handle,
    mode_w: u32,
    mode_h: u32,
) -> std::io::Result<Option<WritebackState>> {
    if let Err(e) = card.set_client_capability(ClientCapability::Atomic, true) {
        return Err(std::io::Error::other(format!(
            "DRM_CLIENT_CAP_ATOMIC unsupported: {e}"
        )));
    }
    if let Err(e) = card.set_client_capability(ClientCapability::WritebackConnectors, true) {
        return Err(std::io::Error::other(format!(
            "DRM_CLIENT_CAP_WRITEBACK_CONNECTORS unsupported: {e}"
        )));
    }

    // Find a writeback connector.
    let mut wb_conn = None;
    for c in res.connectors() {
        let info = match card.get_connector(*c, false) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if info.interface() == connector::Interface::Writeback {
            wb_conn = Some(*c);
            break;
        }
    }
    let wb_conn = match wb_conn {
        Some(c) => c,
        None => return Ok(None),
    };

    // Resolve property handles by name.
    let props = card.get_properties(wb_conn)?;
    let (prop_handles, _vals) = props.as_props_and_values();
    let mut wb_fb_id = None;
    let mut wb_out_fence_ptr = None;
    let mut crtc_id_prop = None;
    for ph in prop_handles {
        let info = match card.get_property(*ph) {
            Ok(i) => i,
            Err(_) => continue,
        };
        match info.name().to_str() {
            Ok("WRITEBACK_FB_ID") => wb_fb_id = Some(*ph),
            Ok("WRITEBACK_OUT_FENCE_PTR") => wb_out_fence_ptr = Some(*ph),
            Ok("CRTC_ID") => crtc_id_prop = Some(*ph),
            _ => {}
        }
    }
    let fb_id_prop = wb_fb_id.ok_or_else(|| {
        std::io::Error::other("writeback connector lacks WRITEBACK_FB_ID property")
    })?;
    let out_fence_ptr_prop = wb_out_fence_ptr.ok_or_else(|| {
        std::io::Error::other("writeback connector lacks WRITEBACK_OUT_FENCE_PTR property")
    })?;
    let crtc_id_prop = crtc_id_prop.ok_or_else(|| {
        std::io::Error::other("writeback connector lacks CRTC_ID property")
    })?;
    let _ = crtc; // silence unused-warning when no probe failure path uses crtc

    // Allocate the persistent target buffer (XR24 = X8R8G8B8 little-endian).
    // VKMS supports XR24/AR24/RG16 for writeback; XR24 matches Xorg's
    // 24-bit truecolor scanout perfectly so the writeback composition is
    // a 1:1 copy.
    let target_buffer =
        card.create_dumb_buffer((mode_w, mode_h), DrmFourcc::Xrgb8888, 32)?;
    let stride = target_buffer.pitch();
    let target_fb = card.add_framebuffer(&target_buffer, 24, 32)?;

    // Run a TEST_ONLY atomic commit so we know the kernel will accept
    // real commits below. This is where DRM master conflicts surface
    // (EACCES) — if Xorg is holding master on this card, atomic commits
    // from us will be rejected. In that case we tear down the target
    // buffer and fall back to the modesetting-FB capture path.
    if let Err(e) = test_writeback_commit(
        card,
        crtc,
        wb_conn,
        crtc_id_prop,
        fb_id_prop,
        out_fence_ptr_prop,
        target_fb,
    ) {
        tracing::warn!(
            "writeback test commit failed ({e}); falling back. \
             this typically means another DRM client (e.g. Xorg) holds master."
        );
        // Tear down what we allocated.
        let _ = card.destroy_framebuffer(target_fb);
        let _ = card.destroy_dumb_buffer(target_buffer);
        return Ok(None);
    }

    Ok(Some(WritebackState {
        connector: wb_conn,
        crtc_id_prop,
        fb_id_prop,
        out_fence_ptr_prop,
        target_buffer,
        target_fb,
        width: mode_w,
        height: mode_h,
        stride,
    }))
}

/// Run a TEST_ONLY atomic commit attaching the writeback connector to
/// `crtc` with `target_fb`. Used to detect master conflicts at startup.
fn test_writeback_commit(
    card: &Card,
    crtc: crtc::Handle,
    wb_conn: connector::Handle,
    crtc_id_prop: property::Handle,
    fb_id_prop: property::Handle,
    out_fence_ptr_prop: property::Handle,
    target_fb: framebuffer::Handle,
) -> std::io::Result<()> {
    let mut probe_fence_fd: i32 = -1;
    let probe_ptr = (&mut probe_fence_fd as *mut i32) as u64;
    let conn_raw = NonZeroU32::new(u32::from(wb_conn))
        .ok_or_else(|| std::io::Error::other("writeback connector handle is zero"))?;
    let mut req = AtomicModeReq::new();
    req.add_raw_property(conn_raw, crtc_id_prop, u64::from(u32::from(crtc)));
    req.add_raw_property(conn_raw, fb_id_prop, u64::from(u32::from(target_fb)));
    req.add_raw_property(conn_raw, out_fence_ptr_prop, probe_ptr);
    card.atomic_commit(
        AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::TEST_ONLY,
        req,
    )?;
    // TEST_ONLY should not actually populate the fence fd, but if it did
    // (kernel quirk) we close it cleanly.
    if probe_fence_fd >= 0 {
        unsafe {
            let _ = OwnedFd::from_raw_fd(probe_fence_fd);
        }
    }
    Ok(())
}

fn capture_via_writeback(
    card: &Card,
    crtc: crtc::Handle,
    wb: &WritebackState,
) -> std::io::Result<(OwnedFd, FbGeometry)> {
    // Build the atomic commit: wire writeback connector to the active CRTC
    // and attach the target FB. WRITEBACK_OUT_FENCE_PTR carries a userspace
    // pointer the kernel will write the sync_file fd into.
    let mut out_fence_fd: i32 = -1;
    let out_fence_ptr_addr = (&mut out_fence_fd as *mut i32) as u64;

    let conn_raw = NonZeroU32::new(u32::from(wb.connector))
        .ok_or_else(|| std::io::Error::other("writeback connector handle is zero"))?;

    let mut req = AtomicModeReq::new();
    req.add_raw_property(conn_raw, wb.crtc_id_prop, u64::from(u32::from(crtc)));
    req.add_raw_property(
        conn_raw,
        wb.fb_id_prop,
        u64::from(u32::from(wb.target_fb)),
    );
    req.add_raw_property(conn_raw, wb.out_fence_ptr_prop, out_fence_ptr_addr);

    // ALLOW_MODESET: enabling/disabling the writeback connector counts as
    // a modeset on every commit (writeback is a one-shot connector).
    card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        .map_err(|e| std::io::Error::other(format!("writeback atomic_commit: {e}")))?;

    if out_fence_fd < 0 {
        return Err(std::io::Error::other(
            "writeback commit succeeded but out-fence fd was not populated by kernel",
        ));
    }
    let fence_fd = unsafe { OwnedFd::from_raw_fd(out_fence_fd) };
    wait_fence(fence_fd.as_fd(), 1000)?;
    drop(fence_fd); // close the sync_file; we no longer need it

    // PRIME-export the GEM handle. The export creates a fresh fd referring
    // to the same DMA-BUF; the GEM handle stays alive for the next capture.
    let prime_fd = card.buffer_to_prime_fd(wb.target_buffer.handle(), 0)?;

    Ok((
        prime_fd,
        FbGeometry {
            width: wb.width,
            height: wb.height,
            stride: wb.stride,
        },
    ))
}

/// Legacy capture path: PRIME-export the framebuffer currently attached
/// to the active CRTC. Works on hardware where the scanout BO is what's
/// actually being written to.
fn capture_via_modesetting_fb(
    card: &Card,
    crtc: crtc::Handle,
) -> std::io::Result<(OwnedFd, FbGeometry)> {
    let crtc_info = card.get_crtc(crtc)?;
    let fb_handle = crtc_info
        .framebuffer()
        .ok_or_else(|| std::io::Error::other("active CRTC has no framebuffer"))?;

    // Try FB2 (drmModeGetFB2) first — handles GBM/multi-plane framebuffers
    // that the modern modesetting driver creates. Fall back to legacy FB1.
    let (width, height, stride, prime_fd) =
        if let Ok(fb2) = card.get_planar_framebuffer(fb_handle) {
            let (w, h) = fb2.size();
            let s = fb2.pitches()[0];
            let buf_handle = fb2.buffers()[0]
                .ok_or_else(|| std::io::Error::other("FB2 plane 0 has no buffer handle"))?;
            let fd = card.buffer_to_prime_fd(buf_handle, 0)?;
            (w, h, s, fd)
        } else {
            let fb_info = card.get_framebuffer(fb_handle)?;
            let (w, h) = fb_info.size();
            let s = fb_info.pitch();
            let buf_handle = fb_info
                .buffer()
                .ok_or_else(|| std::io::Error::other("FB1 has no backing buffer handle"))?;
            let fd = card.buffer_to_prime_fd(buf_handle, 0)?;
            (w, h, s, fd)
        };

    Ok((
        prime_fd,
        FbGeometry {
            width,
            height,
            stride,
        },
    ))
}

fn open_first_card() -> std::io::Result<Card> {
    for i in 0..10u32 {
        let path = format!("/dev/dri/card{i}");
        match File::options().read(true).write(true).open(&path) {
            Ok(f) => {
                tracing::debug!(path, "opened DRM card");
                return Ok(Card(f));
            }
            Err(_) => continue,
        }
    }
    Err(std::io::Error::other(
        "no DRM card found under /dev/dri/card0..9",
    ))
}

/// Wait for a sync_file fd to signal POLLIN.
fn wait_fence(fd: BorrowedFd<'_>, timeout_ms: i32) -> std::io::Result<()> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if r == 0 {
        return Err(std::io::Error::other("writeback fence wait timed out"));
    }
    if pfd.revents & libc::POLLIN == 0 {
        return Err(std::io::Error::other(format!(
            "writeback fence poll returned revents=0x{:x}",
            pfd.revents
        )));
    }
    Ok(())
}
