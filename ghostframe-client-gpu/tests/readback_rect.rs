// SPDX-License-Identifier: MIT OR Apache-2.0
//! `begin_texture_read` against a real device: a rectangle at an offset, and
//! a copy that is ordered at the call, not at the first poll. Needs a GPU; not
//! named in any CI workflow.

use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_render::readback::{begin_texture_read, block_on_device};

fn texture(ctx: &WgpuContext, w: u32, h: u32) -> wgpu::Texture {
    ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn write(ctx: &WgpuContext, t: &wgpu::Texture, w: u32, h: u32, pixels: &[u8]) {
    ctx.queue.write_texture(
        t.as_image_copy(),
        pixels,
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
}

#[test]
fn a_rectangle_reads_back_exactly_and_the_padding_is_gone() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let (w, h) = (70u32, 5u32); // 280 bytes a row: not a multiple of 256
    let t = texture(&ctx, w, h);
    let pixels: Vec<u8> = (0..(w * h * 4)).map(|i| (i % 251) as u8).collect();
    write(&ctx, &t, w, h, &pixels);
    let whole = block_on_device(
        &ctx.device,
        begin_texture_read(&ctx.device, &ctx.queue, &t, 0, 0, w, h).finish(),
    )
    .expect("read");
    assert_eq!(whole, pixels, "the whole texture");
    let rect = block_on_device(
        &ctx.device,
        begin_texture_read(&ctx.device, &ctx.queue, &t, 3, 2, 10, 2).finish(),
    )
    .expect("read rect");
    let want: Vec<u8> = (2..4)
        .flat_map(|y| pixels[((y * w + 3) * 4) as usize..((y * w + 13) * 4) as usize].to_vec())
        .collect();
    assert_eq!(rect, want, "an offset rectangle");
}

#[test]
fn the_copy_is_ordered_at_the_call_not_at_the_first_poll() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let t = texture(&ctx, 4, 1);
    write(&ctx, &t, 4, 1, &[1; 16]);
    let before = begin_texture_read(&ctx.device, &ctx.queue, &t, 0, 0, 4, 1);
    write(&ctx, &t, 4, 1, &[2; 16]); // after the call, before anything is awaited
    let after = begin_texture_read(&ctx.device, &ctx.queue, &t, 0, 0, 4, 1);
    let got_before = block_on_device(&ctx.device, before.finish()).unwrap();
    let got_after = block_on_device(&ctx.device, after.finish()).unwrap();
    assert_eq!(
        got_before,
        vec![1; 16],
        "a copy submitted before the second write sees the first"
    );
    assert_eq!(got_after, vec![2; 16]);
}
