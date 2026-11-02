//! Tests that require a real GPU. Deliberately NOT named in any CI
//! workflow: CI runners have no suitable device, and the project rule is
//! that CI exempts itself in the workflow file rather than the test
//! carrying an #[ignore] that would hide it from developers too.

use ghostframe_client_gpu::dmabuf::dma_buf_write_pending;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;

#[test]
fn wgpu_device_can_export_memory_as_a_file_descriptor() {
    let ctx = WgpuContext::new().expect("create wgpu context with fd export");

    // The minimum that makes a dmabuf possible at all. Note this is
    // VULKAN_EXTERNAL_MEMORY_FD, NOT the combined
    // VULKAN_EXTERNAL_MEMORY_DMA_BUF: the latter additionally demands
    // VK_EXT_image_drm_format_modifier, which RADV on Polaris does not
    // ship, and which is only needed to negotiate an explicit tiling.
    //
    // There is no GLES equivalent to assert here: dmabuf export on that
    // backend is `EGL_MESA_image_dma_buf_export`, a MESA extension wgpu has
    // no feature bit for. `WgpuContext::new` checks the EGL extension string
    // instead and returns `AdapterCannotExport` without it, so reaching this
    // point at all is the assertion on that backend.
    #[cfg(feature = "vulkan")]
    assert!(
        ctx.device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD),
        "device lacks VULKAN_EXTERNAL_MEMORY_FD"
    );

    // Explicit-modifier support is optional and merely recorded. This
    // assertion documents the machine the suite ran on rather than
    // demanding a capability; Task 3 takes the LINEAR path when false.
    eprintln!(
        "explicit DRM format modifiers: {} (adapter: {})",
        ctx.explicit_modifiers,
        ctx.adapter.get_info().name
    );

    // And it must still be a usable wgpu device.
    let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("spike"),
        size: 256,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&buf, 0, &[0xABu8; 256]);
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert_eq!(slice.get_mapped_range().expect("get_mapped_range")[0], 0xAB);
}

#[cfg(feature = "vulkan")]
#[test]
fn raw_vulkan_device_is_reachable_for_the_export_path() {
    // Task 3 needs the raw VkDevice to allocate exportable images, because
    // wgpu-hal 30 offers dmabuf IMPORT but no export. Prove the escape
    // hatch is reachable before building on it.
    let ctx = WgpuContext::new().expect("wgpu context");
    let reached = ctx.with_raw_device(|_raw_device, _phys| true);
    assert_eq!(
        reached,
        Some(true),
        "could not reach the raw VkDevice via as_hal"
    );
}

/// The GLES sibling of the above, and for the same reason: wgpu-hal cannot
/// build a texture over a dmabuf we allocated, so `export_gles.rs` reaches EGL
/// and GL directly.
///
/// Asserts the *whole* hatch, not just that a pointer came back: that the EGL
/// instance resolves, that the display is real, and that every entry point the
/// export path calls can be loaded through it. A driver that advertises the
/// extension string and then fails to resolve one of its functions is a real
/// configuration, and it would otherwise surface as a null call mid-export.
///
/// The list tracks what `export_gles.rs` actually loads. It named the
/// `eglExportDMABUFImage*MESA` pair while the export went the other way
/// (wgpu texture -> dmabuf); that direction is gone -- panfrost only ever
/// exported AFBC -- so those are no longer the functions to check.
#[cfg(feature = "gles")]
#[test]
fn raw_egl_is_reachable_and_exports_resolve() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let resolved = ctx
        .with_raw_egl(|egl_ctx| {
            assert!(
                !egl_ctx.display.as_ptr().is_null(),
                "EGLDisplay is null despite with_raw_egl succeeding"
            );
            [
                "eglCreateImageKHR",
                "eglDestroyImageKHR",
                "glGenTextures",
                "glBindTexture",
                "glDeleteTextures",
                "glTexParameteri",
                "glEGLImageTargetTexture2DOES",
            ]
            .into_iter()
            .filter(|name| egl_ctx.egl.get_proc_address(name).is_none())
            .collect::<Vec<_>>()
        })
        .expect("could not reach EGL via as_hal");
    assert!(
        resolved.is_empty(),
        "EGL/GL entry points the export path needs did not resolve: {resolved:?}"
    );
}

use ghostframe_client_gpu::export::ExportedImage;

#[test]
fn exported_image_yields_a_usable_dmabuf_fd_and_layout() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let img = ExportedImage::new(&ctx, 256, 128, &[], true).expect("export image");

    assert_eq!(img.width, 256);
    assert_eq!(img.height, 128);
    assert!(!img.planes.is_empty(), "no plane layout reported");
    assert!(img.raw_fd() >= 0, "invalid dmabuf fd");
    // Stride must cover the row. A zero stride is the classic symptom of
    // reading the layout off the wrong subresource aspect.
    assert!(
        img.planes[0].stride >= 256 * 4,
        "implausible stride {}",
        img.planes[0].stride
    );
    // On a driver without VK_EXT_image_drm_format_modifier the only
    // possible answer is LINEAR.
    if !ctx.explicit_modifiers {
        assert_eq!(
            img.modifier, 0,
            "linear path must report DRM_FORMAT_MOD_LINEAR"
        );
    }
}

#[test]
fn unsatisfiable_modifier_preference_fails_loudly() {
    let ctx = WgpuContext::new().expect("wgpu context");
    // A reserved-invalid modifier no device supports.
    let err = ExportedImage::new(&ctx, 64, 64, &[0x00ff_ffff_ffff_fffe], true);
    assert!(
        matches!(
            err,
            Err(ghostframe_client_gpu::GpuError::NoCommonModifier { .. })
        ),
        "expected NoCommonModifier, got {err:?}"
    );
}

#[test]
fn exported_fd_is_a_real_dmabuf() {
    // A plausible fd number proves nothing -- verify the kernel agrees it
    // is a dma_buf, or a bug that returns some other fd passes silently.
    let ctx = WgpuContext::new().expect("wgpu context");
    let img = ExportedImage::new(&ctx, 64, 64, &[], true).expect("export image");
    let link = std::fs::read_link(format!("/proc/self/fd/{}", img.raw_fd()))
        .expect("read /proc/self/fd link");
    assert!(
        link.to_string_lossy().contains("dmabuf"),
        "fd is not a dmabuf: {link:?}"
    );
}

#[test]
fn exported_image_can_be_wrapped_as_a_wgpu_texture() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let img = ExportedImage::new(&ctx, 64, 64, &[], true).expect("export image");
    let tex = img
        .as_wgpu_texture(&ctx.device)
        .expect("wrap as wgpu texture");

    assert_eq!(tex.width(), 64);
    assert_eq!(tex.height(), 64);
    // BGRA: see `in_export`.
    assert_eq!(tex.format(), wgpu::TextureFormat::Bgra8Unorm);
}

/// The bytes an RGBA colour occupies in an exported dmabuf.
///
/// The export is `bgra8unorm` -- X11/DRI3 infers a dmabuf's layout from depth
/// and bpp and there is no depth meaning RGBA, so exporting RGBA put red and
/// blue the wrong way round and a blue desktop rendered orange. See
/// `export_gles.rs`'s module doc.
///
/// Spelled as a conversion rather than by writing the swapped literals inline,
/// so the assertions below still read in the colour the test set. It is
/// deliberately NOT applied inside `read_export`: doing the swap on the way out
/// would mask a genuine channel-order regression, which is the exact defect
/// this ordering exists to prevent.
fn in_export(rgba: [u8; 4]) -> [u8; 4] {
    [rgba[2], rgba[1], rgba[0], rgba[3]]
}

/// Read an export back row-major, whichever way the backend allows.
///
/// Bytes are in the EXPORT's order (BGRA), not RGBA -- compare against
/// [`in_export`].
///
/// Returns `(bytes, stride, base_offset)`.
///
/// **Two paths, and the difference matters.** On a linear export this mmaps the
/// dmabuf itself, which is the stronger assertion: it proves the pixels really
/// are in the buffer the consumer will import. On a non-linear one that is
/// impossible -- a row-major read of tiled or compressed data returns
/// plausible-looking garbage, which is exactly how three of these tests failed
/// when the GLES export still produced AFBC -- so it falls back to a wgpu
/// texture copy, which proves the blit landed but cannot speak for the dmabuf's
/// byte layout.
///
/// Since the GLES backend allocates LINEAR through GBM, both backends should
/// now take the mmap path, and the printed line below is how you notice if one
/// silently stops.
///
/// Which path ran is printed, because a test that silently downgrades what it
/// proves is worse than one that fails.
fn read_export(ctx: &WgpuContext, img: &ExportedImage) -> (Vec<u8>, usize, usize) {
    const DRM_FORMAT_MOD_LINEAR: u64 = 0;
    if img.modifier == DRM_FORMAT_MOD_LINEAR {
        let bytes = img.map_read().expect("map dmabuf");
        return (
            bytes,
            img.planes[0].stride as usize,
            img.planes[0].offset as usize,
        );
    }

    eprintln!(
        "read_export: modifier 0x{:016x} is tiled (vendor 0x{:02x}), so the dmabuf \
         cannot be read row-major; verifying through a wgpu texture copy instead. \
         This checks the blit landed, NOT the dmabuf's byte layout. See \
         export_gles.rs's module doc.",
        img.modifier,
        (img.modifier >> 56) & 0xff,
    );

    let tex = img
        .as_wgpu_texture(&ctx.device)
        .expect("wrap export as texture");
    // wgpu requires COPY_BYTES_PER_ROW_ALIGNMENT (256) on bytes_per_row.
    let unpadded = img.width as usize * 4;
    let stride = unpadded.div_ceil(256) * 256;
    let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("export-readback"),
        size: (stride * img.height as usize) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(img.height),
            },
        },
        wgpu::Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        },
    );
    ctx.queue.submit(std::iter::once(encoder.finish()));
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll after readback copy");
    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll for map");
    let bytes = slice.get_mapped_range().expect("get_mapped_range").to_vec();
    (bytes, stride, 0)
}

use ghostframe_client_gpu::framebuffer::Framebuffer;
use ghostframe_client_gpu::ring::CopyPath;

#[test]
fn framebuffer_blits_into_the_exported_dmabuf() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);

    // A colour that cannot be confused with zeroed memory or with a
    // channel-order mistake: R, G and B all differ.
    fb.debug_fill(&ctx.device, &ctx.queue, [0x11, 0x22, 0x33, 0xFF]);

    let img = ExportedImage::new(&ctx, 64, 64, &[], true).expect("export image");
    let tex = img.as_wgpu_texture(&ctx.device).expect("wrap");
    fb.blit_full(&ctx.device, &ctx.queue, &tex);
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let (bytes, stride, base) = read_export(&ctx, &img);

    // Check a pixel away from the origin: an origin-only check passes even
    // when the stride is wrong.
    let off = base + 40 * stride + 20 * 4;
    assert_eq!(
        &bytes[off..off + 4],
        &in_export([0x11, 0x22, 0x33, 0xFF]),
        "wrong pixel at (20,40); channel order or stride is wrong"
    );
}

#[test]
fn framebuffer_preserves_content_across_resize() {
    // Mirrors the web client's preserve-on-resize copy. Without it, tiles
    // written before a late sentinel-driven resize are lost.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0x44, 0x55, 0x66, 0xFF]);

    fb.resize(&ctx.device, &ctx.queue, 128, 96);
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    assert_eq!(fb.width, 128);
    assert_eq!(fb.height, 96);
    let bytes = fb.debug_read(&ctx.device, &ctx.queue);
    let off = (10 * 128 + 10) * 4; // inside the preserved region
    assert_eq!(
        &bytes[off..off + 4],
        &[0x44, 0x55, 0x66, 0xFF],
        "resize did not preserve existing content"
    );
}

use ghostframe_client_gpu::ring::ExportRing;

#[test]
fn ring_partial_update_preserves_untouched_regions() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 3, &[], true).expect("ring");

    // Frame 1: whole surface red, into buffer A.
    fb.debug_fill(&ctx.device, &ctx.queue, [0xFF, 0x00, 0x00, 0xFF]);
    ring.mark_dirty_all();
    let a = ring.publish(&ctx, &fb).expect("publish 1");

    // Frame 2: paint ONLY tile (1,0) green, publish into a different buffer.
    fb.debug_fill_tile(&ctx.device, &ctx.queue, 1, 0, [0x00, 0xFF, 0x00, 0xFF]);
    ring.mark_dirty(1, 0);
    let b = ring.publish(&ctx, &fb).expect("publish 2");
    assert_ne!(
        a.buffer_id, b.buffer_id,
        "must not reuse a buffer still held"
    );

    let img = ring.buffer(b.buffer_id);
    let (bytes, stride, base) = read_export(&ctx, img);

    // The newly painted tile starts at pixel x=32.
    assert_eq!(
        &bytes[base + 32 * 4..base + 32 * 4 + 4],
        &in_export([0x00, 0xFF, 0x00, 0xFF]),
        "new tile not copied"
    );
    // THE POINT: buffer B was never written before, so it must have received
    // frame 1's red as well -- not just frame 2's single green tile.
    assert_eq!(
        &bytes[base..base + 4],
        &in_export([0xFF, 0x00, 0x00, 0xFF]),
        "untouched region lost: partial blit ignored buffer history"
    );
    // And a row far away, to catch a stride mistake.
    assert_eq!(
        &bytes[base + 40 * stride..base + 40 * stride + 4],
        &in_export([0xFF, 0x00, 0x00, 0xFF]),
        "untouched row lost"
    );
}

#[test]
fn publish_returns_none_when_every_buffer_is_held() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[], true).expect("ring");
    ring.mark_dirty_all();
    assert!(ring.publish(&ctx, &fb).is_some());
    ring.mark_dirty_all();
    assert!(ring.publish(&ctx, &fb).is_some());
    ring.mark_dirty_all();
    // Nothing free: the library keeps decoding privately, it does not stall.
    assert!(ring.publish(&ctx, &fb).is_none());
}

#[test]
fn released_buffers_are_recycled() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[], true).expect("ring");
    ring.mark_dirty_all();
    let a = ring.publish(&ctx, &fb).expect("a");
    ring.mark_dirty_all();
    let b = ring.publish(&ctx, &fb).expect("b");
    ring.release(a.frame_id);
    ring.release(b.frame_id);
    ring.mark_dirty_all();
    let c = ring.publish(&ctx, &fb).expect("c");
    assert!(c.buffer_id == a.buffer_id || c.buffer_id == b.buffer_id);
}

#[test]
fn releasing_an_unknown_frame_id_is_ignored() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[], true).expect("ring");
    ring.release(4242); // must not panic
}

#[test]
fn recycled_buffer_receives_only_its_damage_and_keeps_the_rest() {
    // The genuine partial-blit path, which the "preserves untouched regions"
    // test above does NOT reach: there the recycled buffer had never been
    // filled, so `union_since(None)` forced a full-surface blit and the
    // assertions passed without any rect coalescing happening at all.
    //
    // A ring of ONE buffer makes recycling deterministic, so the second
    // publish must reuse a buffer whose `filled_at_gen` is already set and
    // therefore takes the `union_since(Some(gen)) -> coalesce` branch.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 1, &[], true).expect("ring");

    // Frame 1: whole surface red, full blit into the only buffer.
    fb.debug_fill(&ctx.device, &ctx.queue, [0xFF, 0x00, 0x00, 0xFF]);
    ring.mark_dirty_all();
    let a = ring.publish(&ctx, &fb).expect("publish 1");
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    ring.release(a.frame_id);

    // Frame 2: one tile green. The buffer is already filled, so this must
    // be a PARTIAL blit of just that tile.
    fb.debug_fill_tile(&ctx.device, &ctx.queue, 1, 0, [0x00, 0xFF, 0x00, 0xFF]);
    ring.mark_dirty(1, 0);
    let b = ring.publish(&ctx, &fb).expect("publish 2");
    assert_eq!(a.buffer_id, b.buffer_id, "one-buffer ring must recycle");

    // Damage must be the single tile, not the whole surface. This is what
    // proves the partial branch ran rather than the full-blit fallback.
    assert_eq!(
        b.damage.len(),
        1,
        "expected one damage rect, got {:?}",
        b.damage
    );
    assert_eq!(
        (b.damage[0].x, b.damage[0].y, b.damage[0].w, b.damage[0].h),
        (32, 0, 32, 32),
        "damage should be exactly tile (1,0) in pixels"
    );

    let img = ring.buffer(b.buffer_id);
    let (bytes, stride, base) = read_export(&ctx, img);

    assert_eq!(
        &bytes[base + 32 * 4..base + 32 * 4 + 4],
        &in_export([0x00, 0xFF, 0x00, 0xFF]),
        "partial blit did not copy the damaged tile"
    );
    assert_eq!(
        &bytes[base..base + 4],
        &in_export([0xFF, 0x00, 0x00, 0xFF]),
        "partial blit clobbered an undamaged region"
    );
    assert_eq!(
        &bytes[base + 40 * stride..base + 40 * stride + 4],
        &in_export([0xFF, 0x00, 0x00, 0xFF]),
        "partial blit clobbered a distant undamaged row"
    );
}

/// A pixel no neighbour shares: x, y and a constant per frame, so a copy from
/// the wrong place, a swapped channel or a lost alpha each change the value.
fn pattern(frame: u8, x: u32, y: u32) -> [u8; 4] {
    [
        (x as u8)
            .wrapping_mul(2)
            .wrapping_add(frame.wrapping_mul(53)),
        (y as u8)
            .wrapping_mul(3)
            .wrapping_add(frame.wrapping_mul(29)),
        frame.wrapping_mul(17) ^ 0x10,
        0x80 | frame,
    ]
}

fn write_pattern(ctx: &WgpuContext, fb: &Framebuffer, frame: u8) {
    let (w, h) = (fb.width, fb.height);
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            data.extend_from_slice(&pattern(frame, x, y));
        }
    }
    ctx.queue.write_texture(
        fb.texture().as_image_copy(),
        &data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
}

/// Publish a full frame of pattern 0, then pattern 1 with only `dirty` tiles
/// marked, into the same buffer, and compare EVERY pixel of the export: tiles
/// in `dirty` must hold pattern 1, everything else must still hold pattern 0.
///
/// The framebuffer holds pattern 1 everywhere, so a copy that strays outside
/// the damage shows up as pattern 1 where pattern 0 is expected.
fn scattered_damage_reaches_exactly_the_damaged_tiles(w: u32, h: u32, dirty: &[(u32, u32)]) {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, w, h);
    let mut ring = ExportRing::new(&ctx, w, h, 1, &[], true).expect("ring");
    #[cfg(feature = "gles")]
    assert!(
        ring.uses_compute_copy(),
        "the compute export copy is not in use here, so this proves nothing about it"
    );

    write_pattern(&ctx, &fb, 0);
    ring.mark_dirty_all();
    let first = ring.publish(&ctx, &fb).expect("publish 1");
    assert_eq!(
        ring.last_copy_path(),
        Some(CopyPath::Blit),
        "a never-filled buffer is copied whole, by the blit"
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    ring.release(first.frame_id);

    write_pattern(&ctx, &fb, 1);
    for &(tx, ty) in dirty {
        ring.mark_dirty(tx, ty);
    }
    let second = ring.publish(&ctx, &fb).expect("publish 2");
    #[cfg(feature = "gles")]
    assert_eq!(
        ring.last_copy_path(),
        Some(CopyPath::Kernel),
        "scattered damage must go through the compute kernel"
    );
    assert_eq!(
        first.buffer_id, second.buffer_id,
        "one-buffer ring recycles"
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let (bytes, stride, base) = read_export(&ctx, ring.buffer(second.buffer_id));
    let mut from_new = 0usize;
    for y in 0..h {
        for x in 0..w {
            let damaged = dirty.contains(&(x / 32, y / 32));
            let want = in_export(pattern(damaged as u8, x, y));
            let at = base + y as usize * stride + x as usize * 4;
            assert_eq!(
                &bytes[at..at + 4],
                &want,
                "pixel ({x},{y}) in tile ({},{}), damaged={damaged}",
                x / 32,
                y / 32
            );
            from_new += damaged as usize;
        }
    }
    // The property above is vacuous if no pixel was supposed to change.
    assert!(from_new > 0, "no damaged pixel was checked");
}

/// One buffer, so every frame is copied over the previous frame's content, and
/// the damage alternates dense (blit) and sparse (kernel) so each path writes
/// over what the other left. Every frame compares every pixel against the
/// frame that last owned its tile.
#[test]
fn switching_between_blit_and_kernel_keeps_every_tile_exact() {
    const W: u32 = 100;
    const H: u32 = 70;
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, W, H);
    let mut ring = ExportRing::new(&ctx, W, H, 1, &[], true).expect("ring");
    let all: Vec<(u32, u32)> = (0..3)
        .flat_map(|ty| (0..4).map(move |tx| (tx, ty)))
        .collect();
    let checker: Vec<(u32, u32)> = all
        .iter()
        .copied()
        .filter(|(x, y)| (x + y) % 2 == 0)
        .collect();
    let steps: Vec<(&[(u32, u32)], CopyPath)> = vec![
        (&all, CopyPath::Blit),
        (&[(0, 0), (3, 2)], CopyPath::Kernel),
        (&all, CopyPath::Blit),
        (&checker, CopyPath::Kernel),
        (&[(1, 1)], CopyPath::Blit),
        (&[(3, 0), (0, 2)], CopyPath::Kernel),
        (&checker, CopyPath::Kernel),
        (&all, CopyPath::Blit),
    ];
    let mut owner = [[0u8; 4]; 3];
    for (n, (dirty, want_path)) in steps.iter().enumerate() {
        let frame = n as u8 + 1;
        write_pattern(&ctx, &fb, frame);
        for &(tx, ty) in dirty.iter() {
            ring.mark_dirty(tx, ty);
            owner[ty as usize][tx as usize] = frame;
        }
        let published = ring.publish(&ctx, &fb).expect("publish");
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        // The first frame is a never-filled buffer: whole, whatever was marked.
        if n == 0 {
            owner = [[frame; 4]; 3];
        }
        #[cfg(feature = "gles")]
        assert_eq!(ring.last_copy_path(), Some(*want_path), "step {n}");
        #[cfg(feature = "vulkan")]
        let _ = want_path;

        let (bytes, stride, base) = read_export(&ctx, ring.buffer(published.buffer_id));
        for y in 0..H {
            for x in 0..W {
                let f = owner[(y / 32) as usize][(x / 32) as usize];
                let at = base + y as usize * stride + x as usize * 4;
                assert_eq!(
                    &bytes[at..at + 4],
                    &in_export(pattern(f, x, y)),
                    "step {n}: pixel ({x},{y}) should still be frame {f}'s"
                );
            }
        }
        ring.release(published.frame_id);
    }
}

#[test]
fn damage_in_opposite_corners_and_a_clipped_edge_tile_is_copied_exactly() {
    // 100x70: four tile columns (the last 4 px wide) and three rows (the last
    // 6 px high), so (3,2) is clipped on both axes and (3,0) and (0,2) on one.
    scattered_damage_reaches_exactly_the_damaged_tiles(100, 70, &[(0, 0), (3, 2), (3, 0)]);
}

#[test]
fn checkerboard_damage_is_copied_exactly() {
    let dirty: Vec<(u32, u32)> = (0..3)
        .flat_map(|ty| (0..4).map(move |tx| (tx, ty)))
        .filter(|(tx, ty)| (tx + ty) % 2 == 0)
        .collect();
    scattered_damage_reaches_exactly_the_damaged_tiles(100, 70, &dirty);
}

/// `true` while `fd` is an open descriptor naming the same file it did when
/// `expect_ino` was recorded. Comparing the inode, not just `F_GETFD`, is
/// what catches the actual defect: a closed fd number silently reused by
/// another open.
fn fd_names(fd: i32, expect_dev_ino: (u64, u64)) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return false;
    }
    (st.st_dev as u64, st.st_ino as u64) == expect_dev_ino
}

fn dev_ino(fd: i32) -> (u64, u64) {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(fd, &mut st) }, 0, "fstat on live fd");
    (st.st_dev as u64, st.st_ino as u64)
}

#[test]
fn resize_while_a_frame_is_held_keeps_its_fd_open_until_release() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[], false).expect("ring");
    ring.mark_dirty_all();
    let held = ring.publish(&ctx, &fb).expect("publish");
    let identity = dev_ino(held.fd);

    ring.resize(&ctx, 128, 96).expect("resize");

    assert_eq!(
        ring.retired_in_flight(),
        1,
        "held buffer must be retired, not dropped"
    );
    assert!(
        fd_names(held.fd, identity),
        "resize closed a dmabuf fd the host still holds"
    );

    // A frame published after the resize must not reuse the held frame's id:
    // the C shim keys its damage list by frame_id.
    let fb2 = Framebuffer::new(&ctx.device, 128, 96);
    ring.mark_dirty_all();
    let fresh = ring.publish(&ctx, &fb2).expect("publish 2");
    assert_ne!(
        fresh.frame_id, held.frame_id,
        "frame_id reused across resize"
    );
    assert_eq!((fresh.width, fresh.height), (128, 96));
    assert!(fd_names(held.fd, identity), "fd lost after a later publish");

    ring.release(held.frame_id);
    assert_eq!(
        ring.retired_in_flight(),
        0,
        "release must drain the graveyard"
    );
    ring.release(held.frame_id); // double release stays harmless
    ring.release(fresh.frame_id);
}

#[test]
fn resize_with_nothing_held_retires_nothing() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[], false).expect("ring");
    ring.resize(&ctx, 96, 96).expect("resize");
    assert_eq!(ring.retired_in_flight(), 0);
}

#[test]
fn a_repeated_frame_dimensions_does_not_retire_held_buffers() {
    // The server sends FrameDimensions 10 times per session; only a real
    // size change may touch the buffers the host is presenting.
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[], false).expect("ring");
    ring.mark_dirty_all();
    let held = ring.publish(&ctx, &fb).expect("publish");
    let identity = dev_ino(held.fd);
    for _ in 0..10 {
        ring.resize(&ctx, 64, 64).expect("resize");
    }
    assert_eq!(ring.retired_in_flight(), 0);
    assert!(fd_names(held.fd, identity));
    ring.release(held.frame_id);
}

#[test]
fn since_previous_is_relative_to_the_previous_publish_not_the_buffer() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 128, 64);
    let mut ring = ExportRing::new(&ctx, 128, 64, 2, &[], true).expect("ring");

    // The first frame has no predecessor to be relative to.
    ring.mark_dirty_all();
    let a = ring.publish(&ctx, &fb).expect("a");
    assert!(a.since_previous.is_none());

    // Second frame goes into the other (never filled) buffer, so its own
    // damage is the whole surface -- but against frame `a` only tile (2,0)
    // changed, and that is what the window needs.
    ring.mark_dirty(2, 0);
    let b = ring.publish(&ctx, &fb).expect("b");
    assert_eq!(b.damage.len(), 1);
    assert_eq!((b.damage[0].w, b.damage[0].h), (128, 64));
    let sp = b.since_previous.expect("b follows a");
    assert_eq!(sp.frame_id, a.frame_id);
    assert_eq!(sp.damage.len(), 1);
    assert_eq!(
        (
            sp.damage[0].x,
            sp.damage[0].y,
            sp.damage[0].w,
            sp.damage[0].h
        ),
        (64, 0, 32, 32)
    );

    // Both buffers are held: this publish fails, but its damage must still
    // be carried into the next successful one.
    ring.mark_dirty(0, 1);
    assert!(ring.publish(&ctx, &fb).is_none());
    ring.release(a.frame_id);
    ring.mark_dirty(3, 1);
    let c = ring.publish(&ctx, &fb).expect("c");
    let sp = c.since_previous.expect("c follows b");
    assert_eq!(sp.frame_id, b.frame_id);
    let mut tiles: Vec<(u32, u32)> = sp.damage.iter().map(|r| (r.x / 32, r.y / 32)).collect();
    tiles.sort_unstable();
    assert_eq!(tiles, vec![(0, 1), (3, 1)]);
}

#[test]
fn since_previous_is_none_after_a_resize() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[], true).expect("ring");
    ring.mark_dirty_all();
    let a = ring.publish(&ctx, &fb).expect("a");
    ring.release(a.frame_id);

    ring.resize(&ctx, 96, 64).expect("resize");
    let fb = Framebuffer::new(&ctx.device, 96, 64);
    ring.mark_dirty_all();
    let b = ring.publish(&ctx, &fb).expect("b");
    // The history was reset, so the pre-resize frame is not a baseline: an
    // empty delta here would mean "nothing changed" and leave stale pixels.
    assert!(b.since_previous.is_none());
}

/// Publish a full-screen frame behind three queued full-screen uploads, so the
/// GPU has real work when `publish` returns. Returns, per round, whether the
/// GPU was still writing the export right after `publish`, having checked the
/// pixels the CPU then reads back.
fn publish_behind_queued_work(
    ring: &mut ExportRing,
    ctx: &WgpuContext,
    rounds: u32,
    scattered: bool,
) -> Vec<bool> {
    const W: u32 = 1920;
    const H: u32 = 1080;
    let mut fb = Framebuffer::new(&ctx.device, W, H);
    let mut still_writing = Vec::new();
    for i in 0..rounds {
        let last = [0x10 + i as u8, 0x22, 0x33, 0xFF];
        fb.debug_fill(&ctx.device, &ctx.queue, [0xFF, 0, 0, 0xFF]);
        fb.debug_fill(&ctx.device, &ctx.queue, [0, 0xFF, 0, 0xFF]);
        fb.debug_fill(&ctx.device, &ctx.queue, last);
        // Scattered: a checkerboard of tiles, which the kernel copies. A buffer
        // new to the ring is copied whole whatever is marked.
        let probes: &[(u32, u32)] = if scattered {
            for ty in 0..H.div_ceil(32) {
                for tx in (0..W.div_ceil(32)).filter(|tx| (tx + ty) % 2 == 0) {
                    ring.mark_dirty(tx, ty);
                }
            }
            // Inside tiles (0,0), (58,32), (30,10) and (2,2): all marked.
            &[
                (5, 5),
                (58 * 32 + 5, 32 * 32 + 5),
                (30 * 32 + 3, 10 * 32 + 3),
                (66, 66),
            ]
        } else {
            ring.mark_dirty_all();
            &[(0, 0), (W - 1, H - 1), (W / 2, H / 3), (7, H - 2)]
        };
        let frame = ring.publish(ctx, &fb).expect("publish");
        if scattered && i >= 2 {
            #[cfg(feature = "gles")]
            assert_eq!(ring.last_copy_path(), Some(CopyPath::Kernel), "round {i}");
        }
        let img = ring.buffer(frame.buffer_id);
        // Straight away, before anything has had a reason to wait.
        still_writing.push(dma_buf_write_pending(img.raw_fd()));

        let (bytes, stride, base) = read_export(ctx, img);
        for &(x, y) in probes {
            let off = base + y as usize * stride + x as usize * 4;
            assert_eq!(
                &bytes[off..off + 4],
                &in_export(last),
                "round {i}: stale or missing pixel at ({x},{y})"
            );
        }
        ring.release(frame.frame_id);
    }
    still_writing
}

/// The property that matters: `publish` hands the buffer over while the GPU is
/// still writing it (it did not block), and a CPU read of it still sees the
/// finished frame (the reservation orders the read behind the blit).
///
/// "At least once in eight" rather than every time, because a fast GPU can
/// finish early on any one frame; a publish that blocks never shows it.
#[test]
fn publish_does_not_block_and_a_cpu_read_still_sees_the_frame() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut ring = ExportRing::new(&ctx, 1920, 1080, 2, &[], true).expect("ring");
    let busy = publish_behind_queued_work(&mut ring, &ctx, 8, false);
    eprintln!("GPU still writing right after publish, per round: {busy:?}");
    if ring.publish_is_nonblocking() {
        assert!(
            busy.iter().any(|b| *b),
            "publish returned with the GPU idle every time: it is waiting for the blit"
        );
    } else {
        eprintln!("this device has no non-blocking publish; the ring waits instead");
    }
}

/// The same, with the kernel doing the copy: scattered damage behind queued
/// work, then a CPU read of the dmabuf.
#[test]
fn kernel_publish_does_not_block_and_a_cpu_read_still_sees_the_frame() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut ring = ExportRing::new(&ctx, 1920, 1080, 2, &[], true).expect("ring");
    let busy = publish_behind_queued_work(&mut ring, &ctx, 8, true);
    eprintln!("GPU still writing right after a kernel publish, per round: {busy:?}");
    if ring.publish_is_nonblocking() {
        assert!(
            busy.iter().any(|b| *b),
            "publish returned with the GPU idle every time: it is waiting for the copy"
        );
    }
}

/// The blocking path (no fence, or `GHOSTFRAME_BLOCKING_PUBLISH`): the buffer is
/// finished when `publish` returns.
#[test]
fn blocking_publish_returns_a_finished_buffer() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut ring = ExportRing::new(&ctx, 1920, 1080, 2, &[], true).expect("ring");
    ring.force_blocking_publish();
    assert!(!ring.publish_is_nonblocking());
    let busy = publish_behind_queued_work(&mut ring, &ctx, 8, false);
    assert!(
        busy.iter().all(|b| !*b),
        "a blocking publish returned with the GPU still writing: {busy:?}"
    );
}
