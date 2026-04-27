//! DRM framebuffer capture via PRIME DMA-BUF export.
//!
//! Opens the first available DRM card, finds the first active CRTC
//! (one with both a mode and an attached framebuffer), exports the
//! framebuffer's backing buffer as a PRIME DMA-BUF fd, and returns
//! the fd together with the framebuffer geometry.

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use drm::control::Device as ControlDevice;
use drm::Device;

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

/// Open the first available DRM render/primary node and export the active
/// framebuffer as a PRIME DMA-BUF.
///
/// Returns `(OwnedFd, FbGeometry)`.  The caller owns the fd and must close
/// it when done (or pass it to `readback_dmabuf`).
///
/// # Errors
/// Returns an error if no DRM card is found, no active CRTC exists, or the
/// kernel rejects the PRIME export (e.g. the driver does not support it).
pub fn capture_prime_fd() -> std::io::Result<(OwnedFd, FbGeometry)> {
    // Try /dev/dri/card0 .. card9
    let card = open_first_card()?;

    let res = card.resource_handles()?;

    // Find first CRTC that has a framebuffer attached (i.e. is active).
    for crtc_handle in res.crtcs() {
        let crtc_info = match card.get_crtc(*crtc_handle) {
            Ok(info) => info,
            Err(_) => continue,
        };

        // Only consider CRTCs with an active mode and framebuffer.
        if crtc_info.mode().is_none() {
            continue;
        }
        let fb_handle = match crtc_info.framebuffer() {
            Some(h) => h,
            None => continue,
        };

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

        tracing::info!(
            width,
            height,
            stride,
            "captured DRM framebuffer as PRIME DMA-BUF"
        );

        return Ok((prime_fd, FbGeometry { width, height, stride }));
    }

    Err(std::io::Error::other("no active DRM CRTC with a framebuffer found"))
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
