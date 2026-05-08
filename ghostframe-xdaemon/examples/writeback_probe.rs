//! VKMS / DRM writeback connector probe.
//!
//! Opens /dev/dri/card0, finds the Writeback connector, allocates a target
//! dumb buffer, performs an atomic commit attaching the connector to the
//! active CRTC with WRITEBACK_FB_ID and WRITEBACK_OUT_FENCE_PTR, waits on
//! the resulting sync_file fence, then mmaps the target buffer and prints
//! a SHA256-style hash (first 16 bytes of an FNV-1a 64-bit hash) of its
//! contents along with the first few pixels.
//!
//! Run twice (under different X11 root contents) to verify the hash
//! actually changes — that proves writeback is composing live framebuffer
//! state into our target buffer.
//!
//! ```bash
//! /usr/local/bin/writeback_probe          # paint root #1
//! xsetroot -solid '#ff0000'
//! /usr/local/bin/writeback_probe          # paint root #2; hash must differ
//! ```

use std::fs::File;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{
    atomic::AtomicModeReq, connector, AtomicCommitFlags, Device as ControlDevice,
};
use drm::{ClientCapability, Device};

struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/dev/dri/card0".to_string());
    eprintln!("opening {path}");
    let card = Card(File::options().read(true).write(true).open(&path)?);

    // Atomic + writeback caps.
    card.set_client_capability(ClientCapability::Atomic, true)
        .map_err(|e| std::io::Error::other(format!("set_client_capability(Atomic): {e}")))?;
    card.set_client_capability(ClientCapability::WritebackConnectors, true)
        .map_err(|e| std::io::Error::other(format!("set_client_capability(WritebackConnectors): {e}")))?;

    let res = card.resource_handles()?;

    // Find the active CRTC (with mode + framebuffer).
    let mut active_crtc = None;
    let mut active_mode = None;
    for c in res.crtcs() {
        let info = match card.get_crtc(*c) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if let (Some(mode), Some(_fb)) = (info.mode(), info.framebuffer()) {
            active_crtc = Some(*c);
            active_mode = Some(mode);
            eprintln!(
                "active CRTC {:?}: {}x{}",
                c, mode.size().0, mode.size().1
            );
            break;
        }
    }
    let active_crtc = active_crtc
        .ok_or_else(|| std::io::Error::other("no active CRTC with mode+FB"))?;
    let mode = active_mode.unwrap();
    let (w, h) = mode.size();
    let (w, h) = (w as u32, h as u32);

    // Find the writeback connector.
    let mut wb_connector = None;
    for c in res.connectors() {
        let info = match card.get_connector(*c, false) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if info.interface() == connector::Interface::Writeback {
            eprintln!("writeback connector: {:?} (interface_id={})",
                      c, info.interface_id());
            wb_connector = Some(*c);
            break;
        }
    }
    let wb_connector = wb_connector
        .ok_or_else(|| std::io::Error::other("no writeback connector"))?;

    // Look up writeback property handles by name.
    let props = card.get_properties(wb_connector)?;
    let (prop_handles, _vals) = props.as_props_and_values();
    let mut wb_fb_id_prop = None;
    let mut wb_out_fence_ptr_prop = None;
    let mut crtc_id_prop = None;
    for ph in prop_handles {
        let info = card.get_property(*ph)?;
        match info.name().to_str() {
            Ok("WRITEBACK_FB_ID") => wb_fb_id_prop = Some(*ph),
            Ok("WRITEBACK_OUT_FENCE_PTR") => wb_out_fence_ptr_prop = Some(*ph),
            Ok("CRTC_ID") => crtc_id_prop = Some(*ph),
            _ => {}
        }
    }
    let wb_fb_id_prop = wb_fb_id_prop
        .ok_or_else(|| std::io::Error::other("WRITEBACK_FB_ID property not found"))?;
    let wb_out_fence_ptr_prop = wb_out_fence_ptr_prop
        .ok_or_else(|| std::io::Error::other("WRITEBACK_OUT_FENCE_PTR property not found"))?;
    let crtc_id_prop = crtc_id_prop
        .ok_or_else(|| std::io::Error::other("CRTC_ID property not found"))?;
    eprintln!(
        "props: WB_FB_ID={:?} WB_OUT_FENCE_PTR={:?} CRTC_ID={:?}",
        wb_fb_id_prop, wb_out_fence_ptr_prop, crtc_id_prop
    );

    // Allocate persistent target dumb buffer (XR24, no alpha).
    let mut dumb = card.create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32)?;
    eprintln!("dumb buffer: {}x{} pitch={}",
              dumb.size().0, dumb.size().1, dumb.pitch());

    // Wrap as FB.
    let target_fb = card.add_framebuffer(&dumb, 24, 32)?;
    eprintln!("target FB: {:?}", target_fb);

    // Sync_file fd will be written here by the kernel.
    let mut out_fence_fd: i32 = -1;
    let out_fence_ptr_addr = (&mut out_fence_fd as *mut i32) as u64;

    // Atomic commit.
    let conn_raw = NonZeroU32::new(u32::from(wb_connector))
        .ok_or_else(|| std::io::Error::other("connector handle is zero"))?;
    let mut req = AtomicModeReq::new();
    req.add_raw_property(
        conn_raw,
        crtc_id_prop,
        u64::from(u32::from(active_crtc)),
    );
    req.add_raw_property(
        conn_raw,
        wb_fb_id_prop,
        u64::from(u32::from(target_fb)),
    );
    req.add_raw_property(
        conn_raw,
        wb_out_fence_ptr_prop,
        out_fence_ptr_addr,
    );

    eprintln!("submitting atomic commit (ALLOW_MODESET)");
    card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)?;
    eprintln!("commit OK; out_fence_fd={out_fence_fd}");

    if out_fence_fd < 0 {
        return Err(std::io::Error::other("out_fence_fd not populated"));
    }

    // Take ownership of the sync_file fd.
    let fence_fd = unsafe { OwnedFd::from_raw_fd(out_fence_fd) };
    wait_fence(fence_fd.as_fd())?;
    eprintln!("fence signalled");

    // Map and hash.
    let mapping = card.map_dumb_buffer(&mut dumb)?;
    let bytes: &[u8] = &mapping;
    let n = bytes.len().min(64 * 1024);
    let mut h = 0xcbf29ce484222325u64; // FNV-1a 64
    for b in &bytes[..n] {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let first_px = if bytes.len() >= 16 {
        format!("{:02x?}", &bytes[..16])
    } else {
        String::from("(<16 bytes)")
    };
    println!("WRITEBACK_HASH={h:016x} bytes={n} first16={first_px}");

    // Cleanup.
    drop(mapping);
    card.destroy_framebuffer(target_fb)?;
    card.destroy_dumb_buffer(dumb)?;
    Ok(())
}

/// Wait for a sync_file fd to signal POLLIN.
fn wait_fence(fd: BorrowedFd<'_>) -> std::io::Result<()> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = 1000;
    let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if r == 0 {
        return Err(std::io::Error::other("fence wait timed out"));
    }
    if pfd.revents & libc::POLLIN == 0 {
        return Err(std::io::Error::other(format!(
            "fence poll revents={:x}",
            pfd.revents
        )));
    }
    Ok(())
}
