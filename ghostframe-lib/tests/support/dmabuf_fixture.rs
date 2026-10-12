//! Real dma-buf fixtures for GPU-pipeline tests.
//!
//! A `memfd` is NOT a dma-buf: Vulkan refuses it as
//! `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` and the tests that used
//! one skipped themselves and reported ok. These fixtures build real dma-bufs.
//!
//! Every constructor PANICS with an instruction when it cannot produce a real
//! dma-buf. It never returns "unsupported": a test that cannot get its input
//! must fail, not pass. CI, which has no `/dev/udmabuf` access assumed,
//! excludes the tests that need these by name prefix `dmabuf_real_`
//! (see `.github/workflows/ci.yml`).
//!
//! This file is shared by `#[path]` between the lib's unit tests and the
//! integration tests, so it depends on `libc` only.
#![allow(dead_code)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const PAGE: usize = 4096;

/// A real dma-buf plus the geometry it was built with.
pub struct TestDmabuf {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// Size of the dma-buf in bytes (page aligned).
    pub size: usize,
    /// Which exporter produced it ("udmabuf", "dumb:/dev/dri/card1", ...).
    pub source: String,
}

impl TestDmabuf {
    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn fail(what: &str, err: impl std::fmt::Display) -> ! {
    panic!(
        "cannot create a real dma-buf ({what}): {err}. This test needs a real dma-buf and must \
         not skip: it requires read/write access to /dev/udmabuf (udev rule or `chmod`). CI \
         excludes tests named `dmabuf_real_*` by name."
    );
}

/// Build the byte image of a frame: `pixel(x, y)` BGRA for x < width, `pad`
/// in the bytes between `width * 4` and `stride`, and `pad` after the last row.
pub fn frame_bytes(
    width: u32,
    height: u32,
    stride: u32,
    pad: u8,
    pixel: impl Fn(u32, u32) -> [u8; 4],
) -> Vec<u8> {
    assert!(stride >= width * 4, "stride < width*4");
    let mut buf = vec![pad; stride as usize * height as usize];
    for y in 0..height {
        for x in 0..width {
            let o = (y * stride + x * 4) as usize;
            buf[o..o + 4].copy_from_slice(&pixel(x, y));
        }
    }
    buf
}

/// Tightly packed copy (`width * 4` per row) of a strided frame.
pub fn tight(bytes: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let o = y * stride as usize;
        out.extend_from_slice(&bytes[o..o + width as usize * 4]);
    }
    out
}

/// Create a dma-buf holding `bytes` via udmabuf (a sealed memfd, page
/// aligned). The dma-buf is `bytes.len()` rounded up to a page.
pub fn udmabuf_from_bytes(bytes: &[u8], width: u32, height: u32, stride: u32) -> TestDmabuf {
    let size = bytes.len().div_ceil(PAGE) * PAGE;
    unsafe {
        let ud = libc::open(c"/dev/udmabuf".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if ud < 0 {
            fail("open /dev/udmabuf", std::io::Error::last_os_error());
        }
        let ud = OwnedFd::from_raw_fd(ud);
        let m = libc::memfd_create(
            c"gf-test-dmabuf".as_ptr(),
            libc::MFD_ALLOW_SEALING | libc::MFD_CLOEXEC,
        );
        if m < 0 {
            fail("memfd_create", std::io::Error::last_os_error());
        }
        let m = OwnedFd::from_raw_fd(m);
        if libc::ftruncate(m.as_raw_fd(), size as i64) != 0 {
            fail("ftruncate", std::io::Error::last_os_error());
        }
        let w = libc::pwrite(m.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0);
        if w < 0 || w as usize != bytes.len() {
            fail("pwrite", std::io::Error::last_os_error());
        }
        if libc::fcntl(m.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) != 0 {
            fail("F_ADD_SEALS", std::io::Error::last_os_error());
        }
        #[repr(C)]
        struct Create {
            memfd: u32,
            flags: u32,
            offset: u64,
            size: u64,
        }
        let mut c = Create {
            memfd: m.as_raw_fd() as u32,
            flags: 1, // UDMABUF_FLAGS_CLOEXEC
            offset: 0,
            size: size as u64,
        };
        // _IOW('u', 0x42, struct udmabuf_create)
        let fd = libc::ioctl(ud.as_raw_fd(), 0x4018_7542, &mut c);
        if fd < 0 {
            fail("UDMABUF_CREATE", std::io::Error::last_os_error());
        }
        TestDmabuf {
            fd: OwnedFd::from_raw_fd(fd),
            width,
            height,
            stride,
            size,
            source: "udmabuf".into(),
        }
    }
}

/// udmabuf frame with a caller-chosen stride; `pad` fills every byte that is
/// not a pixel (the pad bytes must never reach the pipeline's output).
pub fn udmabuf_frame(
    width: u32,
    height: u32,
    stride: u32,
    pad: u8,
    pixel: impl Fn(u32, u32) -> [u8; 4],
) -> TestDmabuf {
    let bytes = frame_bytes(width, height, stride, pad, pixel);
    udmabuf_from_bytes(&bytes, width, height, stride)
}

/// Solid-colour udmabuf frame, tight stride.
pub fn udmabuf_solid(width: u32, height: u32, px: [u8; 4]) -> TestDmabuf {
    udmabuf_frame(width, height, width * 4, 0, |_, _| px)
}

/// udmabuf from tightly packed BGRA pixels, laid out at `stride`.
pub fn udmabuf_from_tight(
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    pad: u8,
) -> TestDmabuf {
    assert_eq!(pixels.len(), width as usize * height as usize * 4);
    udmabuf_frame(width, height, stride, pad, |x, y| {
        let o = (y * width + x) as usize * 4;
        [pixels[o], pixels[o + 1], pixels[o + 2], pixels[o + 3]]
    })
}

/// A DRM dumb buffer exported with PRIME (`card` e.g. "/dev/dri/card1"). The
/// kernel chooses the pitch; the buffer is filled through mmap. Panics if the
/// card cannot be opened or the buffer cannot be created.
pub fn dumb_dmabuf(
    card: &str,
    width: u32,
    height: u32,
    pad: u8,
    pixel: impl Fn(u32, u32) -> [u8; 4],
) -> TestDmabuf {
    #[repr(C)]
    #[derive(Default)]
    struct CreateDumb {
        height: u32,
        width: u32,
        bpp: u32,
        flags: u32,
        handle: u32,
        pitch: u32,
        size: u64,
    }
    #[repr(C)]
    struct PrimeHandle {
        handle: u32,
        flags: u32,
        fd: i32,
    }
    unsafe {
        let path = std::ffi::CString::new(card).unwrap();
        let cfd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if cfd < 0 {
            fail(&format!("open {card}"), std::io::Error::last_os_error());
        }
        let cfd = OwnedFd::from_raw_fd(cfd);
        let mut cd = CreateDumb {
            height,
            width,
            bpp: 32,
            ..Default::default()
        };
        // DRM_IOCTL_MODE_CREATE_DUMB
        if libc::ioctl(cfd.as_raw_fd(), 0xC020_64B2u32 as _, &mut cd) < 0 {
            fail("DRM CREATE_DUMB", std::io::Error::last_os_error());
        }
        let mut ph = PrimeHandle {
            handle: cd.handle,
            flags: (libc::O_CLOEXEC | libc::O_RDWR) as u32,
            fd: -1,
        };
        // DRM_IOCTL_PRIME_HANDLE_TO_FD
        if libc::ioctl(cfd.as_raw_fd(), 0xC00C_642Du32 as _, &mut ph) < 0 {
            fail("DRM PRIME_HANDLE_TO_FD", std::io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(ph.fd);
        let size = cd.size as usize;
        let bytes = frame_bytes(width, height, cd.pitch, pad, pixel);
        let p = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        );
        if p == libc::MAP_FAILED {
            fail("mmap dumb buffer", std::io::Error::last_os_error());
        }
        let sync = |flags: u64| {
            libc::ioctl(fd.as_raw_fd(), 0x4008_6200, &flags);
        };
        sync(3); // START | RW
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len().min(size));
        sync(3 | 4); // END | RW
        libc::munmap(p, size);
        // The GEM handle dies with `cfd`; the PRIME fd keeps the buffer alive.
        TestDmabuf {
            fd,
            width,
            height,
            stride: cd.pitch,
            size,
            source: format!("dumb:{card}"),
        }
    }
}
