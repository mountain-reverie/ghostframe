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
