//! NV12 dmabuf import against a real GPU. **GLES backend only.**
//!
//! The GLES counterpart to `gpu_import.rs`, in its own file because the two
//! share the shape of the oracle and none of its machinery — that one reaches
//! into `ash::vk` for layout queries and Vulkan mappings, this one allocates
//! through GBM and writes through `mmap`.
//!
//! The question both answer is the same, and it is not "did I get textures of
//! the right size": **did `import_nv12` place each plane's pixels at the
//! offsets and pitches the descriptor claims?** `renderer.rs` cannot tell the
//! difference — a wrong luma offset renders a plausible picture of the wrong
//! part of memory — so only reading the imported textures back through the GPU
//! proves it.
//!
//! Requires a GPU with `EGL_MESA_image_dma_buf_export` and
//! `EGL_EXT_image_dma_buf_import`. Deliberately not named in any CI workflow:
//! no runner has a suitable device, and the project rule is that CI exempts
//! itself from hardware tests rather than pretending to run them. Run with
//! `just test-client-gles` on the reference machine.
#![cfg(feature = "gles")]

use ghostframe_client_gpu::export::ExportedImage;
use ghostframe_client_gpu::import::import_nv12;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_h264::{DmabufPlanes, PlaneDesc, DRM_FORMAT_GR88, DRM_FORMAT_R8};

const LUMA_W: u32 = 64;
const LUMA_H: u32 = 64;
const CHROMA_W: u32 = 32;
const CHROMA_H: u32 = 32;

/// Row pitch used for both planes.
///
/// Deliberately wider than `LUMA_W`, because a real decoder's pitch is padded
/// and an import that computes `offset + y * width` instead of
/// `offset + y * pitch` shears the image. With pitch == width that bug is
/// invisible.
const PITCH: u32 = 128;

/// Where luma starts. **Deliberately not 0.**
///
/// A real NV12 buffer's luma offset usually *is* 0, which lets an import that
/// hardcodes 0 — or forgets to pass `EGL_DMA_BUF_PLANE0_OFFSET_EXT` at all —
/// pass unnoticed. Reserving a region ahead of luma makes that bug read back
/// as the reserved filler instead of the pattern.
const LUMA_OFFSET: u32 = PITCH * 4;
const CHROMA_OFFSET: u32 = LUMA_OFFSET + PITCH * LUMA_H;

/// Filler in the reserved region, chosen to be a value neither pattern
/// produces, so reading it back is unambiguous rather than merely wrong.
const RESERVED_FILLER: u8 = 0xAB;

fn gpu_or_skip() -> Option<WgpuContext> {
    match WgpuContext::new() {
        Ok(ctx) => Some(ctx),
        Err(e) => {
            eprintln!("no usable GPU ({e}); skipping");
            None
        }
    }
}

/// `luma[y][x] = x ^ y`, `chroma[y][x] = (0x40 + x, 0x80 + y)`.
///
/// Two different patterns, each a function of position, so a swapped plane, a
/// shifted offset and a wrong pitch all produce distinguishable failures rather
/// than one generic mismatch.
fn luma_at(x: u32, y: u32) -> u8 {
    (x ^ y) as u8
}
fn chroma_at(x: u32, y: u32) -> (u8, u8) {
    ((0x40 + x) as u8, (0x80 + y) as u8)
}

/// A dmabuf holding an NV12 image at the offsets above, plus the descriptor
/// that claims so.
///
/// The backing buffer is allocated as ARGB8888 through the export path — the
/// only LINEAR dmabuf allocator this backend has — and then described as NV12.
/// That mismatch is deliberate and is the point: nothing may depend on the
/// allocation's own format or pitch, only on the descriptor.
fn build_test_dmabuf(ctx: &WgpuContext) -> (ExportedImage, DmabufPlanes) {
    let total = CHROMA_OFFSET + PITCH * CHROMA_H;
    // 4 bytes per pixel at ARGB8888, so this width gives a row of `PITCH`
    // bytes; the height covers `total` with room to spare.
    let src = ExportedImage::new(ctx, PITCH / 4, total.div_ceil(PITCH) + 4, &[], true)
        .expect("allocate a LINEAR dmabuf to stage the NV12 image in");

    let size = src
        .planes
        .first()
        .map(|p| p.offset + p.stride * u64::from(src.height))
        .expect("the export reports at least one plane");
    assert!(
        u64::from(total) <= size,
        "test setup bug: need {total} bytes, the allocation is {size}"
    );

    write_patterns(src.raw_fd(), size as usize);

    let planes = DmabufPlanes {
        fd: src.raw_fd(),
        modifier: 0,
        size,
        width: LUMA_W,
        height: LUMA_H,
        luma: PlaneDesc {
            offset: u64::from(LUMA_OFFSET),
            pitch: u64::from(PITCH),
        },
        chroma: PlaneDesc {
            offset: u64::from(CHROMA_OFFSET),
            pitch: u64::from(PITCH),
        },
        fourcc_luma: DRM_FORMAT_R8,
        fourcc_chroma: DRM_FORMAT_GR88,
    };
    (src, planes)
}

/// Write the reserved filler and both patterns straight into the dmabuf.
fn write_patterns(fd: i32, size: usize) {
    // SAFETY: `fd` is a LINEAR dmabuf this test just allocated and still owns,
    // mapped writable for `size` bytes (the length the export reported) and
    // unmapped before returning.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED, "mmap of the staging dmabuf failed");
    // SAFETY: `base` is a valid writable mapping of `size` bytes.
    let buf = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, size) };

    buf.fill(RESERVED_FILLER);
    for y in 0..LUMA_H {
        for x in 0..LUMA_W {
            buf[(LUMA_OFFSET + y * PITCH + x) as usize] = luma_at(x, y);
        }
    }
    for y in 0..CHROMA_H {
        for x in 0..CHROMA_W {
            let (cb, cr) = chroma_at(x, y);
            let at = (CHROMA_OFFSET + y * PITCH + x * 2) as usize;
            buf[at] = cb;
            buf[at + 1] = cr;
        }
    }
    // SAFETY: the pair returned by the `mmap` above.
    unsafe { libc::munmap(base, size) };
}

fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
) -> Vec<u8> {
    let unpadded = width * bytes_per_pixel;
    let padded =
        unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("gles-import-test-readback"),
        size: padded as u64 * height as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("gles-import-test-readback"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback buffer"));
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll for the readback");
    let padded_bytes = slice
        .get_mapped_range()
        .expect("map the readback range")
        .to_vec();

    // Drop the padding so the caller compares pixels, not stride.
    let mut out = Vec::with_capacity((unpadded * height) as usize);
    for row in 0..height as usize {
        let start = row * padded as usize;
        out.extend_from_slice(&padded_bytes[start..start + unpadded as usize]);
    }
    out
}

/// The oracle: both planes read back exactly what was written at the
/// descriptor's offsets and pitches.
#[test]
fn imports_a_linear_dmabuf_and_reads_the_bytes_back() {
    let Some(ctx) = gpu_or_skip() else { return };
    let (_src, planes) = build_test_dmabuf(&ctx);

    assert_eq!(planes.chroma_width(), CHROMA_W);
    assert_eq!(planes.chroma_height(), CHROMA_H);

    let imported = import_nv12(&ctx, &planes).expect("import the staged NV12 dmabuf");
    assert_eq!(imported.luma().width(), LUMA_W);
    assert_eq!(imported.luma().height(), LUMA_H);
    assert_eq!(imported.chroma().width(), CHROMA_W);
    assert_eq!(imported.chroma().height(), CHROMA_H);

    let luma = read_back(&ctx.device, &ctx.queue, imported.luma(), LUMA_W, LUMA_H, 1);
    for y in 0..LUMA_H {
        for x in 0..LUMA_W {
            let got = luma[(y * LUMA_W + x) as usize];
            assert_eq!(
                got,
                luma_at(x, y),
                "luma ({x},{y}) read back {got:#04x}, expected {:#04x}{}",
                luma_at(x, y),
                if got == RESERVED_FILLER {
                    " -- that is the reserved filler, so the plane offset was ignored"
                } else {
                    ""
                }
            );
        }
    }

    let chroma = read_back(
        &ctx.device,
        &ctx.queue,
        imported.chroma(),
        CHROMA_W,
        CHROMA_H,
        2,
    );
    for y in 0..CHROMA_H {
        for x in 0..CHROMA_W {
            let at = ((y * CHROMA_W + x) * 2) as usize;
            let (want_cb, want_cr) = chroma_at(x, y);
            assert_eq!(
                (chroma[at], chroma[at + 1]),
                (want_cb, want_cr),
                "chroma ({x},{y}) read back {:#04x},{:#04x}, expected {want_cb:#04x},{want_cr:#04x}",
                chroma[at],
                chroma[at + 1]
            );
        }
    }
}

/// A tiled modifier is refused, not imported as if it were linear.
///
/// Importing a tiled buffer as linear does not fail -- it renders a scrambled
/// picture, which is the failure mode this whole descriptor is checked against.
#[test]
fn a_tiled_modifier_is_rejected() {
    let Some(ctx) = gpu_or_skip() else { return };
    let (_src, mut planes) = build_test_dmabuf(&ctx);
    // An arbitrary non-linear, non-INVALID modifier.
    planes.modifier = 0x0100_0000_0000_0002;
    let err = import_nv12(&ctx, &planes).expect_err("a tiled modifier must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("modifier"),
        "the error should name the modifier, got: {msg}"
    );
}

/// A plane that runs past the end of the buffer is refused.
///
/// EGL may or may not notice; this is the check that does not depend on which.
#[test]
fn a_plane_past_the_end_of_the_buffer_is_rejected() {
    let Some(ctx) = gpu_or_skip() else { return };
    let (_src, mut planes) = build_test_dmabuf(&ctx);
    planes.chroma.offset = planes.size - 1;
    let err = import_nv12(&ctx, &planes).expect_err("an out-of-bounds plane must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("past the dmabuf"),
        "the error should say the plane overruns the allocation, got: {msg}"
    );
}

/// The composed `DRM_FORMAT_NV12` fourcc is refused rather than imported as a
/// single plane.
///
/// `DmabufPlanes` normalises to the per-plane pair, so this should be
/// unreachable -- but EGL would *accept* a composed fourcc on plane 0 and
/// sample it wrongly, which is why it is checked rather than assumed.
#[test]
fn a_composed_nv12_fourcc_is_rejected() {
    let Some(ctx) = gpu_or_skip() else { return };
    let (_src, mut planes) = build_test_dmabuf(&ctx);
    planes.fourcc_luma = ghostframe_client_h264::DRM_FORMAT_NV12;
    let err = import_nv12(&ctx, &planes).expect_err("a composed fourcc must be refused");
    assert!(err.to_string().contains("per-plane fourccs"), "got: {err}");
}

/// A texture cloned out of the import outlives it and stays usable.
///
/// The lifetime claim `ImportedNv12`'s doc makes: each texture owns its
/// `EGLImage` through its own drop callback, so dropping the `ImportedNv12`
/// must not tear down a clone's storage. If it did, this read-back would
/// return garbage or crash.
#[test]
fn a_cloned_texture_outlives_the_import_and_stays_usable() {
    let Some(ctx) = gpu_or_skip() else { return };
    let (_src, planes) = build_test_dmabuf(&ctx);

    let clone = {
        let imported = import_nv12(&ctx, &planes).expect("import");
        imported.luma().clone()
    };

    let luma = read_back(&ctx.device, &ctx.queue, &clone, LUMA_W, LUMA_H, 1);
    for y in 0..LUMA_H {
        for x in 0..LUMA_W {
            assert_eq!(
                luma[(y * LUMA_W + x) as usize],
                luma_at(x, y),
                "cloned luma ({x},{y}) changed after the import was dropped"
            );
        }
    }
}
