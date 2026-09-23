//! Tests that require a real GPU. Deliberately NOT named in any CI
//! workflow: CI runners have no suitable device, and the project rule is
//! that CI exempts itself in the workflow file rather than the test
//! carrying an #[ignore] that would hide it from developers too.

use ghostframe_client_gpu::wgpu_ctx::WgpuContext;

#[test]
fn wgpu_device_can_export_memory_as_a_file_descriptor() {
    let ctx = WgpuContext::new().expect("create wgpu context with fd export");

    // The minimum that makes a dmabuf possible at all. Note this is
    // VULKAN_EXTERNAL_MEMORY_FD, NOT the combined
    // VULKAN_EXTERNAL_MEMORY_DMA_BUF: the latter additionally demands
    // VK_EXT_image_drm_format_modifier, which RADV on Polaris does not
    // ship, and which is only needed to negotiate an explicit tiling.
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

use ghostframe_client_gpu::export::ExportedImage;

#[test]
fn exported_image_yields_a_usable_dmabuf_fd_and_layout() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let img = ExportedImage::new(&ctx, 256, 128, &[]).expect("export image");

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
    let err = ExportedImage::new(&ctx, 64, 64, &[0x00ff_ffff_ffff_fffe]);
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
    let img = ExportedImage::new(&ctx, 64, 64, &[]).expect("export image");
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
    let img = ExportedImage::new(&ctx, 64, 64, &[]).expect("export image");
    let tex = img
        .as_wgpu_texture(&ctx.device)
        .expect("wrap as wgpu texture");

    assert_eq!(tex.width(), 64);
    assert_eq!(tex.height(), 64);
    assert_eq!(tex.format(), wgpu::TextureFormat::Rgba8Unorm);
}

use ghostframe_client_gpu::framebuffer::Framebuffer;

#[test]
fn framebuffer_blits_into_the_exported_dmabuf() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);

    // A colour that cannot be confused with zeroed memory or with a
    // channel-order mistake: R, G and B all differ.
    fb.debug_fill(&ctx.device, &ctx.queue, [0x11, 0x22, 0x33, 0xFF]);

    let img = ExportedImage::new(&ctx, 64, 64, &[]).expect("export image");
    let tex = img.as_wgpu_texture(&ctx.device).expect("wrap");
    fb.blit_full(&ctx.device, &ctx.queue, &tex);
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let bytes = img.map_read().expect("map dmabuf");
    let stride = img.planes[0].stride as usize;
    let base = img.planes[0].offset as usize;

    // Check a pixel away from the origin: an origin-only check passes even
    // when the stride is wrong.
    let off = base + 40 * stride + 20 * 4;
    assert_eq!(
        &bytes[off..off + 4],
        &[0x11, 0x22, 0x33, 0xFF],
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
    let mut ring = ExportRing::new(&ctx, 64, 64, 3, &[]).expect("ring");

    // Frame 1: whole surface red, into buffer A.
    fb.debug_fill(&ctx.device, &ctx.queue, [0xFF, 0x00, 0x00, 0xFF]);
    ring.mark_dirty_all();
    let a = ring
        .publish(&ctx.device, &ctx.queue, &fb)
        .expect("publish 1");
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    // Frame 2: paint ONLY tile (1,0) green, publish into a different buffer.
    fb.debug_fill_tile(&ctx.device, &ctx.queue, 1, 0, [0x00, 0xFF, 0x00, 0xFF]);
    ring.mark_dirty(1, 0);
    let b = ring
        .publish(&ctx.device, &ctx.queue, &fb)
        .expect("publish 2");
    assert_ne!(
        a.buffer_id, b.buffer_id,
        "must not reuse a buffer still held"
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let img = ring.buffer(b.buffer_id);
    let bytes = img.map_read().expect("map");
    let stride = img.planes[0].stride as usize;
    let base = img.planes[0].offset as usize;

    // The newly painted tile starts at pixel x=32.
    assert_eq!(
        &bytes[base + 32 * 4..base + 32 * 4 + 4],
        &[0x00, 0xFF, 0x00, 0xFF],
        "new tile not copied"
    );
    // THE POINT: buffer B was never written before, so it must have received
    // frame 1's red as well -- not just frame 2's single green tile.
    assert_eq!(
        &bytes[base..base + 4],
        &[0xFF, 0x00, 0x00, 0xFF],
        "untouched region lost: partial blit ignored buffer history"
    );
    // And a row far away, to catch a stride mistake.
    assert_eq!(
        &bytes[base + 40 * stride..base + 40 * stride + 4],
        &[0xFF, 0x00, 0x00, 0xFF],
        "untouched row lost"
    );
}

#[test]
fn publish_returns_none_when_every_buffer_is_held() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[]).expect("ring");
    ring.mark_dirty_all();
    assert!(ring.publish(&ctx.device, &ctx.queue, &fb).is_some());
    ring.mark_dirty_all();
    assert!(ring.publish(&ctx.device, &ctx.queue, &fb).is_some());
    ring.mark_dirty_all();
    // Nothing free: the library keeps decoding privately, it does not stall.
    assert!(ring.publish(&ctx.device, &ctx.queue, &fb).is_none());
}

#[test]
fn released_buffers_are_recycled() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[]).expect("ring");
    ring.mark_dirty_all();
    let a = ring.publish(&ctx.device, &ctx.queue, &fb).expect("a");
    ring.mark_dirty_all();
    let b = ring.publish(&ctx.device, &ctx.queue, &fb).expect("b");
    ring.release(a.frame_id);
    ring.release(b.frame_id);
    ring.mark_dirty_all();
    let c = ring.publish(&ctx.device, &ctx.queue, &fb).expect("c");
    assert!(c.buffer_id == a.buffer_id || c.buffer_id == b.buffer_id);
}

#[test]
fn releasing_an_unknown_frame_id_is_ignored() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut ring = ExportRing::new(&ctx, 64, 64, 2, &[]).expect("ring");
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
    let mut ring = ExportRing::new(&ctx, 64, 64, 1, &[]).expect("ring");

    // Frame 1: whole surface red, full blit into the only buffer.
    fb.debug_fill(&ctx.device, &ctx.queue, [0xFF, 0x00, 0x00, 0xFF]);
    ring.mark_dirty_all();
    let a = ring
        .publish(&ctx.device, &ctx.queue, &fb)
        .expect("publish 1");
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    ring.release(a.frame_id);

    // Frame 2: one tile green. The buffer is already filled, so this must
    // be a PARTIAL blit of just that tile.
    fb.debug_fill_tile(&ctx.device, &ctx.queue, 1, 0, [0x00, 0xFF, 0x00, 0xFF]);
    ring.mark_dirty(1, 0);
    let b = ring
        .publish(&ctx.device, &ctx.queue, &fb)
        .expect("publish 2");
    assert_eq!(a.buffer_id, b.buffer_id, "one-buffer ring must recycle");
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

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
    let bytes = img.map_read().expect("map");
    let stride = img.planes[0].stride as usize;
    let base = img.planes[0].offset as usize;

    assert_eq!(
        &bytes[base + 32 * 4..base + 32 * 4 + 4],
        &[0x00, 0xFF, 0x00, 0xFF],
        "partial blit did not copy the damaged tile"
    );
    assert_eq!(
        &bytes[base..base + 4],
        &[0xFF, 0x00, 0x00, 0xFF],
        "partial blit clobbered an undamaged region"
    );
    assert_eq!(
        &bytes[base + 40 * stride..base + 40 * stride + 4],
        &[0xFF, 0x00, 0x00, 0xFF],
        "partial blit clobbered a distant undamaged row"
    );
}
