//! Import a dmabuf we created ourselves, and read the imported textures'
//! bytes back through the GPU -- the only way to prove `import_nv12` placed
//! each plane's pixels at the offsets and pitches the descriptor claims,
//! rather than merely returning textures of the right dimensions.
//!
//! Requires a GPU. Deliberately NOT named in any CI workflow: CI runners
//! have no suitable device, and the project rule is that CI exempts itself
//! in the workflow file rather than a test silently skipping at runtime,
//! which would hide a broken GPU from developers too -- see `.expect(...)`
//! below rather than the `let Ok(..) = .. else { return }` an earlier
//! revision of this test used.
//!
//! The dmabuf itself is built with `export.rs`'s existing LINEAR export
//! path, not a decoder, so this runs with no VA-API involved: a single RGBA
//! image is allocated purely as a source of real, page-backed dmabuf bytes,
//! and this file writes directly into it with its own `mmap`, reinterpreting
//! two distinct byte ranges inside it as an R8 luma plane and an RG8 chroma
//! plane at a deliberately different, nonzero offset and their own
//! (independently Vulkan-queried, not guessed) row pitches. That exercises
//! the one nontrivial part of the design -- two planes, two offsets, two
//! pitches, sharing one allocation -- without needing real NV12 content.

use ash::vk;
use ghostframe_client_gpu::export::ExportedImage;
use ghostframe_client_gpu::import::import_nv12;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_gpu::GpuError;
use ghostframe_client_h264::{DmabufPlanes, PlaneDesc};

const LUMA_W: u32 = 64;
const LUMA_H: u32 = 64;
const CHROMA_W: u32 = 32;
const CHROMA_H: u32 = 32;

/// What Vulkan actually requires for a `tiling` image of this exact shape:
/// its `vkGetImageMemoryRequirements` result and its `vkGetImageSubresourceLayout`
/// row pitch. Queried by creating a real (throwaway, immediately destroyed)
/// image rather than guessed, so the offsets/pitches this test hands to
/// `import_nv12` are the same numbers `import_nv12` will independently
/// recompute for its own same-shaped images -- not values that happen to
/// agree with them.
fn query_layout(
    ctx: &WgpuContext,
    width: u32,
    height: u32,
    format: vk::Format,
) -> (vk::MemoryRequirements, u64) {
    ctx.with_raw_device(|device, _phys| {
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external);
        // SAFETY: `info` is fully populated; `device` is a live handle for
        // the duration of this call.
        let image = unsafe { device.create_image(&info, None) }.expect("create throwaway image");
        // SAFETY: `image` was just created above.
        let req = unsafe { device.get_image_memory_requirements(image) };
        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            array_layer: 0,
        };
        // SAFETY: as above; `image` is unbound, which `check_pitch`'s own
        // SAFETY note (import.rs) establishes is fine for this query.
        let layout = unsafe { device.get_image_subresource_layout(image, subresource) };
        // SAFETY: `image` is ours alone and nothing else references it.
        unsafe { device.destroy_image(image, None) };
        (req, layout.row_pitch)
    })
    .expect("raw Vulkan device reachable")
}

fn round_up(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}

fn luma_pixel(x: u32, y: u32) -> u8 {
    (x.wrapping_mul(7) ^ y.wrapping_mul(13)).wrapping_add(5) as u8
}

fn chroma_pixel(x: u32, y: u32) -> (u8, u8) {
    (
        (x.wrapping_mul(3) + y.wrapping_mul(5) + 41) as u8,
        (x.wrapping_mul(11) + y.wrapping_mul(2) + 97) as u8,
    )
}

/// Write the luma and chroma patterns directly into the dmabuf backing
/// `fd`, at `luma_pitch`/`chroma_offset`/`chroma_pitch`. `len` must be a
/// safe (<=) bound on the dmabuf's real size -- `fd` is mapped read-write
/// for exactly that many bytes.
fn write_patterns(
    fd: i32,
    len: usize,
    luma_offset: u64,
    luma_pitch: u64,
    chroma_offset: u64,
    chroma_pitch: u64,
) {
    // SAFETY: `fd` is a live dmabuf fd for the duration of this call, freshly
    // exported by this same process and not yet imported by Vulkan, so
    // nothing else is reading or writing it concurrently. `len` is a
    // caller-checked lower bound on the dmabuf's real size.
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        assert_ne!(
            ptr,
            libc::MAP_FAILED,
            "mmap failed: {}",
            std::io::Error::last_os_error()
        );
        let base = ptr as *mut u8;

        for y in 0..LUMA_H as u64 {
            for x in 0..LUMA_W as u64 {
                let off = luma_offset + y * luma_pitch + x;
                *base.add(off as usize) = luma_pixel(x as u32, y as u32);
            }
        }
        for y in 0..CHROMA_H as u64 {
            for x in 0..CHROMA_W as u64 {
                let (u, v) = chroma_pixel(x as u32, y as u32);
                let off = chroma_offset + y * chroma_pitch + x * 2;
                *base.add(off as usize) = u;
                *base.add(off as usize + 1) = v;
            }
        }

        assert_eq!(
            libc::munmap(ptr, len),
            0,
            "munmap failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Read `texture` back through the GPU into a tightly-packed `Vec<u8>`,
/// stripping wgpu's `COPY_BYTES_PER_ROW_ALIGNMENT` row padding. Mirrors
/// `Framebuffer::debug_read`.
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
    let buffer_size = padded as u64 * height as u64;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("gpu-import-test-readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("gpu-import-test-readback"),
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

    // This poll is what makes `padded_bytes` below safe to read -- it is
    // not what makes dropping `imported` safe. `ImportedNv12` has no such
    // lifetime contract (see its struct doc in import.rs): wgpu's own
    // resource tracking keeps the real image (and, through it, the imported
    // memory) alive for as long as anything -- a clone, a view, a bind
    // group -- references it, regardless of whether this poll has run.
    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback buffer"));
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");

    let padded_bytes = slice.get_mapped_range().expect("get_mapped_range");
    let mut tight = Vec::with_capacity(unpadded as usize * height as usize);
    for row in 0..height as usize {
        let start = row * padded as usize;
        let end = start + unpadded as usize;
        tight.extend_from_slice(&padded_bytes[start..end]);
    }
    tight
}

/// Builds the dmabuf described above: `export.rs`'s LINEAR path produces a
/// real, over-provisioned RGBA dmabuf purely as a source of page-backed
/// bytes; this writes an R8 luma pattern at a nonzero offset and an RG8
/// chroma pattern at a second, distinct, alignment-correct offset into it,
/// and returns the `ExportedImage` (kept alive to keep the fd valid) and
/// the `DmabufPlanes` describing it.
fn build_test_dmabuf(ctx: &WgpuContext) -> (ExportedImage, DmabufPlanes) {
    let (luma_req, luma_pitch) = query_layout(ctx, LUMA_W, LUMA_H, vk::Format::R8_UNORM);
    let (chroma_req, chroma_pitch) = query_layout(ctx, CHROMA_W, CHROMA_H, vk::Format::R8G8_UNORM);

    // Neither offset is 0: a real decoder output's luma offset commonly IS
    // 0, but a test that hardcodes that lets a bug that hardcodes 0 in the
    // luma bind (instead of using `planes.luma.offset`) pass unnoticed --
    // this pads a reserved region ahead of luma so a hardcoded-0 bind reads
    // padding, not the pattern, and the round-trip assertions below catch
    // it. `luma_req.alignment` is trivially a multiple of itself, so this
    // stays alignment-correct without another `round_up`.
    let luma_offset = luma_req.alignment;
    let chroma_offset = round_up(luma_offset + luma_req.size, chroma_req.alignment);
    assert!(
        chroma_offset > luma_offset,
        "test setup bug: chroma must land at a distinct offset, further from \
         the start of the buffer than luma's"
    );
    let total = chroma_offset + chroma_req.size;

    // A generously over-provisioned RGBA export: its real backing dmabuf
    // only needs to be at least `total` bytes, and a 64-wide, 512-tall
    // RGBA8 image is comfortably larger than the few tens of KB `total`
    // works out to for 64x64/32x32 planes -- there is no relationship
    // between this image's own pitch/format and the luma/chroma layout
    // written into it below; it exists purely to produce a real dmabuf fd.
    // (A tight-vs-generous backing size was also tried empirically while
    // developing this test, to see whether a too-small declared allocation
    // would become observable through sizing alone; it did not, on either
    // size, because RADV's dma-buf import uses the fd's real backing size
    // regardless of the declared `allocationSize`. That is why `import.rs`
    // guards this by single-sourcing the value it validates and the value
    // it allocates, plus an `lseek` cross-check against the fd's own
    // account of its size, rather than by anything this test could size
    // its way into exercising.)
    let src = ExportedImage::new(ctx, 64, 512, &[], true).expect("export backing dmabuf");

    write_patterns(
        src.raw_fd(),
        total as usize,
        luma_offset,
        luma_pitch,
        chroma_offset,
        chroma_pitch,
    );

    let planes = DmabufPlanes {
        fd: src.raw_fd(),
        modifier: src.modifier,
        size: total,
        fourcc_luma: ghostframe_client_h264::DRM_FORMAT_R8,
        fourcc_chroma: ghostframe_client_h264::DRM_FORMAT_GR88,
        width: LUMA_W,
        height: LUMA_H,
        luma: PlaneDesc {
            offset: luma_offset,
            pitch: luma_pitch,
        },
        chroma: PlaneDesc {
            offset: chroma_offset,
            pitch: chroma_pitch,
        },
    };

    (src, planes)
}

#[test]
fn imports_a_linear_dmabuf_and_reads_the_bytes_back() {
    let ctx = WgpuContext::new().expect("create wgpu context");
    let (_src, planes) = build_test_dmabuf(&ctx);
    assert_eq!(
        planes.chroma_width(),
        CHROMA_W,
        "test setup bug: chroma_width() must match the hand-written CHROMA_W"
    );
    assert_eq!(planes.chroma_height(), CHROMA_H);

    let imported = match import_nv12(&ctx, &planes) {
        Ok(i) => i,
        Err(e) => {
            panic!(
                "import of a LINEAR dmabuf this process just exported failed: {e}. \
                 This is the import path itself failing, not the decoder."
            );
        }
    };
    assert_eq!(imported.luma().width(), LUMA_W);
    assert_eq!(imported.luma().height(), LUMA_H);
    assert_eq!(imported.chroma().width(), CHROMA_W);
    assert_eq!(imported.chroma().height(), CHROMA_H);

    let luma_bytes = read_back(&ctx.device, &ctx.queue, imported.luma(), LUMA_W, LUMA_H, 1);
    for y in 0..LUMA_H {
        for x in 0..LUMA_W {
            let idx = (y * LUMA_W + x) as usize;
            assert_eq!(
                luma_bytes[idx],
                luma_pixel(x, y),
                "luma pixel ({x},{y}) mismatch -- offset/pitch arithmetic is wrong"
            );
        }
    }

    let chroma_bytes = read_back(
        &ctx.device,
        &ctx.queue,
        imported.chroma(),
        CHROMA_W,
        CHROMA_H,
        2,
    );
    for y in 0..CHROMA_H {
        for x in 0..CHROMA_W {
            let idx = ((y * CHROMA_W + x) * 2) as usize;
            let (u, v) = chroma_pixel(x, y);
            assert_eq!(
                (chroma_bytes[idx], chroma_bytes[idx + 1]),
                (u, v),
                "chroma pixel ({x},{y}) mismatch -- offset/pitch arithmetic is wrong"
            );
        }
    }

    // `imported` does NOT need to outlive the GPU operations that read its
    // textures -- see `a_cloned_texture_outlives_the_import_and_stays_
    // usable` below, which drops the import before any GPU work has even
    // been submitted against a clone of one of its textures, and is still
    // sound. Dropped here anyway simply because this test has no further
    // use for it.
    drop(imported);
}

#[test]
fn a_pitch_that_disagrees_with_the_driver_is_rejected_not_silently_sheared() {
    let ctx = WgpuContext::new().expect("create wgpu context");
    let (_src, mut planes) = build_test_dmabuf(&ctx);

    // Corrupt the descriptor's claimed luma pitch so it disagrees with what
    // the driver's own linear image reports -- exactly the situation
    // `check_pitch` exists to catch (module doc, import.rs).
    let true_pitch = planes.luma.pitch;
    planes.luma.pitch += 4;

    let err = import_nv12(&ctx, &planes)
        .expect_err("a wrong pitch must be rejected, not imported and left to shear the image");
    match err {
        GpuError::PitchMismatch {
            plane,
            dmabuf,
            driver,
        } => {
            assert_eq!(plane, "luma");
            assert_eq!(dmabuf, true_pitch + 4);
            assert_eq!(driver, true_pitch);
        }
        other => panic!("expected GpuError::PitchMismatch, got {other:?}"),
    }
}

#[test]
fn a_misaligned_chroma_offset_is_rejected() {
    let ctx = WgpuContext::new().expect("create wgpu context");
    let (_src, mut planes) = build_test_dmabuf(&ctx);

    // `build_test_dmabuf` rounds the chroma offset up to the driver's
    // required alignment and sizes `planes.size` tightly (offset +
    // required size, exactly) -- so *increasing* the offset by any amount
    // would also trip `check_plane_fits` (I3's extent guard), not
    // specifically this one. Subtracting 1 keeps `offset + required_size`
    // comfortably inside `planes.size` while still breaking alignment
    // (verified on this hardware: chroma's alignment is 256, so 16383 is
    // not a multiple of it), isolating the alignment guard as the only
    // thing that can reject this import. The pattern was written at the
    // *correctly* aligned offset, so this only tests that the guard fires
    // -- not the pattern round-trip.
    planes.chroma.offset -= 1;

    let err = import_nv12(&ctx, &planes)
        .expect_err("a misaligned chroma offset must be rejected before binding");
    // `matches!(err, GpuError::Vulkan(_))` alone is not enough:
    // `check_plane_fits` (the extent guard) also returns `GpuError::Vulkan`,
    // and is exactly the check that fired here by mistake once before, when
    // this test increased the offset instead of decreasing it. Asserting
    // the message names the alignment problem specifically is what would
    // have caught that mixup, and is what stands between this test and
    // silently passing for the wrong reason again after a future change to
    // `build_test_dmabuf`'s sizing.
    let msg = err.to_string();
    assert!(
        matches!(err, GpuError::Vulkan(_))
            && msg.contains("chroma plane offset")
            && msg.contains("not a multiple of the required alignment"),
        "expected the alignment guard's own message, got: {msg}"
    );
}

#[test]
fn a_cloned_texture_outlives_the_import_and_stays_usable() {
    // The regression test C3's fix exists for: under the earlier
    // per-struct `Drop` design, `ImportedNv12::drop` destroyed both
    // `VkImage`s and freed their memory synchronously, regardless of what
    // else still referenced them. A caller that cloned a texture (or built
    // a view/bind group from one) and then dropped the import -- exactly
    // what the plan's own Task 9 reference snippet did, dropping the
    // import right after `draw`, which only submits and never waits --
    // would have a live `wgpu::Texture` pointing at an already-destroyed
    // `VkImage`. That is a use-after-free with no validation layers on
    // this machine to turn it into a clean, loud error; it would present
    // as silent corruption or an intermittent GPU hang instead. The fix
    // (`wrap_texture`'s real per-image drop callback, `Arc<ImportOwner>`
    // shared between both) makes the destroy wait for wgpu's own resource
    // tracking to decide nothing references the image anymore, so this
    // must now be sound.
    let ctx = WgpuContext::new().expect("create wgpu context");
    let (_src, planes) = build_test_dmabuf(&ctx);
    let imported = import_nv12(&ctx, &planes).expect("import");

    // Take an owning clone before dropping the import -- the exact
    // operation the struct doc says must be safe.
    let luma_clone = imported.luma().clone();

    // Drop the import now, before any GPU work has touched `luma_clone` --
    // the worst-case ordering for the old design.
    drop(imported);

    // The clone must still be a fully usable texture. A real GPU readback
    // is what actually exercises the hazard rather than merely
    // type-checking it: if the VkImage were already destroyed, this
    // `copy_texture_to_buffer` reads through a dangling handle.
    let luma_bytes = read_back(&ctx.device, &ctx.queue, &luma_clone, LUMA_W, LUMA_H, 1);
    for y in 0..LUMA_H {
        for x in 0..LUMA_W {
            let idx = (y * LUMA_W + x) as usize;
            assert_eq!(
                luma_bytes[idx],
                luma_pixel(x, y),
                "clone read wrong bytes at ({x},{y}) after the import was dropped"
            );
        }
    }
}
