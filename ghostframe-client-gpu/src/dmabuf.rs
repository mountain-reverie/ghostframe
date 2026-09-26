//! The parts of dmabuf export that are not specific to a graphics API.
//!
//! Extracted from `export.rs` when the GLES backend arrived, because all of
//! this is about *dmabufs* and the kernel, not about Vulkan or GL: the plane
//! layout the consumer needs, `DMA_BUF_IOCTL_SYNC` for the CPU readback path,
//! and the modifier negotiation, which is a pure set operation over two lists.
//!
//! Both `export.rs` (Vulkan) and `export_gles.rs` use these. Keeping the
//! modifier choice here in particular matters: it is the one piece with unit
//! tests that need no GPU at all, and duplicating it per backend would mean
//! two chances to get the consumer's preference order wrong.

use crate::GpuError;

/// `DRM_FORMAT_MOD_LINEAR` — the tiling every consumer can import, and so the
/// fallback whenever there is nothing better to agree on.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Byte layout of one dmabuf plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    pub offset: u64,
    pub stride: u64,
}

/// Pick a DRM format modifier both sides can use.
///
/// `supported` is what this device can produce; `preferred` is the consumer's
/// list, most-preferred first. An empty `preferred` means "library picks",
/// which chooses `DRM_FORMAT_MOD_LINEAR` when available because that is the
/// one every consumer can import.
///
/// The consumer's *order* wins over the device's — a consumer that lists a
/// tiled modifier first is telling us it would rather not do the detiling,
/// and the device offering several is indifferent between them.
pub fn choose_modifier(supported: &[u64], preferred: &[u64]) -> Option<u64> {
    if preferred.is_empty() {
        if supported.contains(&DRM_FORMAT_MOD_LINEAR) {
            return Some(DRM_FORMAT_MOD_LINEAR);
        }
        return supported.first().copied();
    }
    preferred.iter().copied().find(|m| supported.contains(m))
}

// ---------------------------------------------------------------------------
// dmabuf CPU-mmap readback (test/diagnostic path -- see each backend's
// `map_read`)
// ---------------------------------------------------------------------------

/// `DMA_BUF_SYNC_READ`, from `linux/dma-buf.h`.
pub const DMA_BUF_SYNC_READ: u64 = 1 << 0;
/// `DMA_BUF_SYNC_START`, from `linux/dma-buf.h`.
pub const DMA_BUF_SYNC_START: u64 = 0;
/// `DMA_BUF_SYNC_END`, from `linux/dma-buf.h`.
pub const DMA_BUF_SYNC_END: u64 = 1 << 2;

/// Mirrors `struct dma_buf_sync` from `linux/dma-buf.h`.
#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

/// `DMA_BUF_IOCTL_SYNC` (`linux/dma-buf.h`): `_IOW(DMA_BUF_BASE, 0, struct
/// dma_buf_sync)`, `DMA_BUF_BASE` is `'b'`, and `struct dma_buf_sync` is a
/// single `__u64 flags` field (8 bytes). `libc` does not carry DMA-BUF's
/// ioctl constants, so the request number is derived here from the
/// `asm-generic/ioctl.h` encoding rather than hard-coded, so the derivation
/// is checkable against the kernel header instead of trusted as a magic
/// number:
///
/// ```text
/// _IOC(dir, type, nr, size) =
///     (dir  << _IOC_DIRSHIFT)  |   // _IOC_DIRSHIFT  = 30
///     (type << _IOC_TYPESHIFT) |   // _IOC_TYPESHIFT = 8
///     (nr   << _IOC_NRSHIFT)   |   // _IOC_NRSHIFT   = 0
///     (size << _IOC_SIZESHIFT)     // _IOC_SIZESHIFT = 16
/// _IOW(type, nr, size) = _IOC(_IOC_WRITE /* 1 */, type, nr, size_of(size))
/// ```
fn dma_buf_ioctl_sync() -> libc::Ioctl {
    const IOC_WRITE: u64 = 1;
    const IOC_NRSHIFT: u64 = 0;
    const IOC_TYPESHIFT: u64 = 8;
    const IOC_SIZESHIFT: u64 = 16;
    const IOC_DIRSHIFT: u64 = 30;

    const DMA_BUF_BASE: u64 = b'b' as u64;
    const NR: u64 = 0;
    const SIZE: u64 = std::mem::size_of::<DmaBufSync>() as u64;

    let request = (IOC_WRITE << IOC_DIRSHIFT)
        | (DMA_BUF_BASE << IOC_TYPESHIFT)
        | (NR << IOC_NRSHIFT)
        | (SIZE << IOC_SIZESHIFT);
    request as libc::Ioctl
}

/// Issue `DMA_BUF_IOCTL_SYNC` with the given flags.
///
/// # Safety
/// `fd` must be a valid, open dmabuf file descriptor.
pub unsafe fn dma_buf_sync(fd: i32, flags: u64) -> Result<(), GpuError> {
    let arg = DmaBufSync { flags };
    // SAFETY: `fd` is a valid dmabuf fd per this function's contract;
    // `arg` is a correctly-shaped `struct dma_buf_sync` for the ioctl's
    // duration.
    let ret = unsafe { libc::ioctl(fd, dma_buf_ioctl_sync(), &arg as *const DmaBufSync) };
    if ret != 0 {
        return Err(GpuError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_preference_prefers_linear() {
        assert_eq!(choose_modifier(&[7, 0, 9], &[]), Some(0));
    }

    #[test]
    fn empty_preference_falls_back_to_first_when_no_linear() {
        assert_eq!(choose_modifier(&[7, 9], &[]), Some(7));
    }

    #[test]
    fn consumer_preference_order_wins_over_device_order() {
        assert_eq!(choose_modifier(&[7, 9], &[9, 7]), Some(9));
    }

    #[test]
    fn no_overlap_is_none() {
        assert_eq!(choose_modifier(&[7, 9], &[1, 2]), None);
    }
}
