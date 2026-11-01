//! Where does the ~0.5ms/tile upload cost actually go?
//!
//! Not a correctness test -- a hardware A/B, run by hand, so a fix for the
//! frame rate is aimed at the real cost instead of a plausible one.
#![cfg(feature = "gles")]

use ghostframe_client_gpu::framebuffer::Framebuffer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use std::time::Instant;

const TILE: u32 = 32;
const W: u32 = 1920;
const H: u32 = 1080;

fn tiles() -> (u32, u32) {
    (W.div_ceil(TILE), H.div_ceil(TILE))
}

#[test]
fn upload_cost_breakdown() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, W, H);
    let (cols, rows) = tiles();
    let n = (cols * rows) as usize;
    let tile_rgba = vec![0x7Au8; (TILE * TILE * 4) as usize];

    // Warm up: first touch of a texture can include lazy allocation.
    for i in 0..64u32 {
        write_one(&ctx, &fb, i % cols, i / cols, &tile_rgba);
    }
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    // A: what the renderer does today -- one write_texture per tile.
    let t0 = Instant::now();
    for i in 0..n {
        let (tx, ty) = (i as u32 % cols, i as u32 / cols);
        write_one(&ctx, &fb, tx, ty, &tile_rgba);
    }
    let submitted = Instant::now();
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let done = Instant::now();

    let issue = submitted.saturating_duration_since(t0);
    let total = done.saturating_duration_since(t0);
    eprintln!(
        "\nA: {n} x write_texture(32x32)\n   issue {:>8.1}ms ({:>6.1}us/tile)\n   +gpu  {:>8.1}ms ({:>6.1}us/tile)",
        issue.as_secs_f64() * 1e3,
        issue.as_secs_f64() * 1e6 / n as f64,
        total.as_secs_f64() * 1e3,
        total.as_secs_f64() * 1e6 / n as f64,
    );

    // B: the whole screen in ONE write_texture. The floor any batching can
    // reach -- same bytes, one call.
    let full = vec![0x7Au8; (W * H * 4) as usize];
    let t0 = Instant::now();
    ctx.queue.write_texture(
        fb.texture().as_image_copy(),
        &full,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(W * 4),
            rows_per_image: Some(H),
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    let submitted = Instant::now();
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let done = Instant::now();
    eprintln!(
        "B: 1 x write_texture(1920x1080)\n   issue {:>8.1}ms\n   +gpu  {:>8.1}ms  ({:>6.1}us/tile-equivalent)\n",
        submitted.saturating_duration_since(t0).as_secs_f64() * 1e3,
        done.saturating_duration_since(t0).as_secs_f64() * 1e3,
        done.saturating_duration_since(t0).as_secs_f64() * 1e6 / n as f64,
    );
}

#[test]
fn coalesced_upload_cost() {
    // What `Renderer::upload_pending_rgba` now produces for a full repaint:
    // the dirty set coalesces to ONE rectangle, so one assembled buffer and
    // one call. Measured separately from B because it includes the CPU cost of
    // assembling the rectangle from per-tile payloads, which B does not pay.
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, W, H);
    let (cols, rows) = tiles();
    let n = (cols * rows) as usize;
    let tile_rgba = vec![0x7Au8; (TILE * TILE * 4) as usize];

    let t0 = Instant::now();
    let mut buf = vec![0u8; (W * H * 4) as usize];
    let dst_row = (W * 4) as usize;
    let src_row = (TILE * 4) as usize;
    for i in 0..n {
        let (tx, ty) = (i as u32 % cols, i as u32 / cols);
        let (dx, dy) = ((tx * TILE) as usize, (ty * TILE) as usize);
        let w = (TILE as usize).min(W as usize - dx);
        let h = (TILE as usize).min(H as usize - dy);
        for row in 0..h {
            let d = (dy + row) * dst_row + dx * 4;
            buf[d..d + w * 4].copy_from_slice(&tile_rgba[row * src_row..row * src_row + w * 4]);
        }
    }
    let assembled = Instant::now();
    ctx.queue.write_texture(
        fb.texture().as_image_copy(),
        &buf,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(W * 4),
            rows_per_image: Some(H),
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let done = Instant::now();
    eprintln!(
        "\nC: assemble {n} tiles into 1 rect + 1 write_texture\n   assemble {:>7.1}ms\n   upload   {:>7.1}ms\n   total    {:>7.1}ms ({:>6.1}us/tile)\n",
        assembled.saturating_duration_since(t0).as_secs_f64() * 1e3,
        done.saturating_duration_since(assembled).as_secs_f64() * 1e3,
        done.saturating_duration_since(t0).as_secs_f64() * 1e3,
        done.saturating_duration_since(t0).as_secs_f64() * 1e6 / n as f64,
    );
}

fn write_one(ctx: &WgpuContext, fb: &Framebuffer, tx: u32, ty: u32, rgba: &[u8]) {
    // 1080 is not a multiple of 32, so the bottom row of tiles is partial --
    // clamp exactly as `upload_rgba_tile` does.
    let h = TILE.min(H.saturating_sub(ty * TILE));
    let w = TILE.min(W.saturating_sub(tx * TILE));
    if w == 0 || h == 0 {
        return;
    }
    ctx.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: fb.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: tx * TILE,
                y: ty * TILE,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        &rgba[..(w * h * 4) as usize],
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

/// Scattered dirty tiles -- no two adjacent, so rectangle coalescing cannot
/// merge any -- and a full repaint, each uploaded both ways: one
/// `write_texture` per rectangle (what `upload_pending_rgba` did) against
/// `RgbaPipeline`'s single buffer upload and dispatch.
#[test]
fn rgba_pipeline_against_write_texture() {
    use ghostframe_client_gpu::pipelines::rgba::RgbaPipeline;

    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, W, H);
    let mut pipeline = RgbaPipeline::new(&ctx.device);
    let (cols, rows) = tiles();
    let tile_rgba = vec![0x7Au8; (TILE * TILE * 4) as usize];
    let wait = || {
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll")
    };
    let ms = |t: Instant| Instant::now().saturating_duration_since(t).as_secs_f64() * 1e3;

    let checker: Vec<(u32, u32)> = (0..rows)
        .flat_map(|y| (0..cols).map(move |x| (x, y)))
        .filter(|(x, y)| (x + y) % 2 == 0)
        .collect();
    let all: Vec<(u32, u32)> = (0..rows)
        .flat_map(|y| (0..cols).map(move |x| (x, y)))
        .collect();

    // Warm both paths up (pipeline compile, buffer growth, lazy allocation).
    let warm: Vec<(u32, u32, &[u8])> = all.iter().map(|&(x, y)| (x, y, &tile_rgba[..])).collect();
    pipeline.draw(&ctx.device, &ctx.queue, &fb, &warm);
    write_one(&ctx, &fb, 0, 0, &tile_rgba);
    wait();

    for (name, set) in [("checkerboard", &checker), ("full repaint", &all)] {
        let n = set.len();
        let t0 = Instant::now();
        if name == "full repaint" {
            // Coalesces to one rectangle: assemble it, then one call.
            let mut buf = vec![0u8; (W * H * 4) as usize];
            for &(tx, ty) in set {
                let (dx, dy) = ((tx * TILE) as usize, (ty * TILE) as usize);
                let w = (TILE as usize).min(W as usize - dx);
                let h = (TILE as usize).min(H as usize - dy);
                for row in 0..h {
                    let d = (dy + row) * (W * 4) as usize + dx * 4;
                    let s = row * (TILE * 4) as usize;
                    buf[d..d + w * 4].copy_from_slice(&tile_rgba[s..s + w * 4]);
                }
            }
            ctx.queue.write_texture(
                fb.texture().as_image_copy(),
                &buf,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(W * 4),
                    rows_per_image: Some(H),
                },
                wgpu::Extent3d {
                    width: W,
                    height: H,
                    depth_or_array_layers: 1,
                },
            );
        } else {
            for &(tx, ty) in set {
                write_one(&ctx, &fb, tx, ty, &tile_rgba);
            }
        }
        let issued_a = ms(t0);
        wait();
        let total_a = ms(t0);

        let batch: Vec<(u32, u32, &[u8])> =
            set.iter().map(|&(x, y)| (x, y, &tile_rgba[..])).collect();
        let t0 = Instant::now();
        pipeline.draw(&ctx.device, &ctx.queue, &fb, &batch);
        let issued_b = ms(t0);
        wait();
        let total_b = ms(t0);

        eprintln!(
            "\n{name} ({n} tiles)\n   write_texture  issue {issued_a:>7.1}ms  +gpu {total_a:>7.1}ms\n   rgba pipeline  issue {issued_b:>7.1}ms  +gpu {total_b:>7.1}ms"
        );
    }
}

/// Where `RgbaPipeline::draw`'s time goes: the buffer upload against the
/// dispatch, at a few batch sizes, repeated to see past first-use costs.
#[test]
fn rgba_pipeline_breakdown() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, W, H);
    let wait = || {
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll")
    };
    for n in [16usize, 256, 1020, 2040] {
        let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (n * 4096) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bytes = vec![0x5Au8; n * 4096];
        for rep in 0..3 {
            let t0 = Instant::now();
            ctx.queue.write_buffer(&buf, 0, &bytes);
            let wrote = Instant::now().saturating_duration_since(t0);
            ctx.queue.submit([]);
            let submitted = Instant::now().saturating_duration_since(t0);
            wait();
            eprintln!(
                "n={n:>4} rep={rep} write_buffer {:>6.2}ms  submit {:>6.2}ms  +gpu {:>6.2}ms",
                wrote.as_secs_f64() * 1e3,
                submitted.as_secs_f64() * 1e3,
                Instant::now().saturating_duration_since(t0).as_secs_f64() * 1e3
            );
        }
    }
    let mut pipeline = ghostframe_client_gpu::pipelines::rgba::RgbaPipeline::new(&ctx.device);
    let tile = vec![0x7Au8; 4096];
    for n in [16usize, 256, 1020, 2040] {
        let batch: Vec<(u32, u32, &[u8])> =
            (0..n as u32).map(|i| (i % 60, i / 60, &tile[..])).collect();
        for rep in 0..3 {
            let t0 = Instant::now();
            pipeline.draw(&ctx.device, &ctx.queue, &fb, &batch);
            let issued = Instant::now().saturating_duration_since(t0);
            wait();
            eprintln!(
                "n={n:>4} rep={rep} draw issue {:>6.2}ms  +gpu {:>6.2}ms",
                issued.as_secs_f64() * 1e3,
                Instant::now().saturating_duration_since(t0).as_secs_f64() * 1e3
            );
        }
    }
}
