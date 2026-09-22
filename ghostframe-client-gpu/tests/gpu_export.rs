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
