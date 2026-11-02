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

/// The staged path against the slab path, per batch size, producer and
/// consumer side separately.
///
/// "staged" is `RgbaPipeline`, what the renderer does with `Vec` tiles that do
/// not coalesce: `produce` is the decode thread building the `Vec`s (allocate,
/// write rows), `issue` is the render thread copying them into wgpu staging and
/// recording and submitting the dispatch. "slab" is `RgbaSlabPipeline`:
/// `produce` is the decode thread writing the same rows into mapped slab
/// memory (plus the sync ioctls), `issue` is the render thread recording the
/// dispatch and writing the small work table. `+gpu` is the time until the
/// device finished. The `bench:` marker lines let a `PAN_MESA_DEBUG=perf` log
/// be split per measurement to count "Gallium flush" lines.
///
/// Run by hand: `cargo test -p ghostframe-client-gpu --no-default-features
/// --features gles,decode-gstreamer,test-support --test upload_bench
/// slab_pipeline_breakdown -- --nocapture --test-threads=1`.
#[test]
fn slab_pipeline_breakdown() {
    use ghostframe_client_core::{TileOut, TileSink};
    use ghostframe_client_gpu::pipelines::rgba::RgbaPipeline;
    use ghostframe_client_gpu::pipelines::rgba_slab::{RgbaSlabPipeline, SlabWork};
    use ghostframe_client_gpu::upload_pool::UploadPool;

    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, W, H);
    let wait = || {
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll")
    };
    let (pool, mut alloc) = UploadPool::new(&ctx).expect("upload pool");
    let mut slab_pipe = RgbaSlabPipeline::new(&ctx.device, &pool);
    let mut staged = RgbaPipeline::new(&ctx.device);
    let row = [0x7Au8; 128];
    // An array assignment, as the decoder does: `copy_from_slice` is a libc
    // `memcpy` call at this crate's opt-level, and a call per 128-byte row into
    // write-combined memory measured 5x slower than the inline store.
    let fill = |out: &mut TileOut<'_>| (0..32).for_each(|y| *out.row_mut(y) = row);
    let ms = |from: Instant| Instant::now().saturating_duration_since(from).as_secs_f64() * 1e3;

    for n in [16usize, 256, 1020, 2040] {
        for rep in 0..4 {
            // --- staged ---
            eprintln!("bench: staged n={n} rep={rep}");
            let t0 = Instant::now();
            let vecs: Vec<Vec<u8>> = (0..n)
                .map(|_| {
                    let mut v = vec![0u8; 4096];
                    fill(&mut TileOut::new(&mut v, 128));
                    v
                })
                .collect();
            let produce = ms(t0);
            let batch: Vec<(u32, u32, &[u8])> = vecs
                .iter()
                .enumerate()
                .map(|(i, v)| (i as u32 % 60, i as u32 / 60, &v[..]))
                .collect();
            let t1 = Instant::now();
            staged.draw(&ctx.device, &ctx.queue, &fb, &batch);
            let issue = ms(t1);
            wait();
            eprintln!(
                "n={n:>4} rep={rep} staged produce {produce:>6.2}ms  issue {issue:>6.2}ms  +gpu {:>6.2}ms",
                ms(t1)
            );
            drop(vecs);

            // --- slab ---
            eprintln!("bench: slab n={n} rep={rep}");
            let t0 = Instant::now();
            let mut by_slab: std::collections::BTreeMap<usize, Vec<SlabWork>> = Default::default();
            let mut counts = vec![0u32; pool.slab_count()];
            for i in 0..n {
                let mut out = alloc.acquire().expect("a free slot");
                fill(&mut out);
                let slot = alloc.commit(0, 0, 0);
                counts[slot.slab()] += 1;
                by_slab.entry(slot.slab()).or_default().push(SlabWork {
                    tile_x: i as u32 % 60,
                    tile_y: i as u32 / 60,
                    slot: slot.index(),
                });
            }
            alloc.end_cpu_writes();
            let produce = ms(t0);
            let batches: Vec<_> = by_slab.into_iter().collect();
            let t1 = Instant::now();
            let mut enc = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            slab_pipe.encode(&ctx.device, &ctx.queue, &mut enc, &fb, &pool, &batches);
            ctx.queue.submit([enc.finish()]);
            let issue = ms(t1);
            wait();
            eprintln!(
                "n={n:>4} rep={rep} slab   produce {produce:>6.2}ms  issue {issue:>6.2}ms  +gpu {:>6.2}ms",
                ms(t1)
            );
            pool.retire_now(&counts);
        }
    }
}

/// Does opening a CPU write window (`DMA_BUF_IOCTL_SYNC` START|WRITE) on a slab
/// wait for GPU jobs that are still sampling earlier slots of it? The decode
/// thread does exactly this: it keeps filling the current slab while the
/// render thread's dispatch over the slots already announced is in flight.
///
/// Submits a full-screen slab dispatch, then without waiting writes more tiles
/// into the same slab and reports the time START and END took
/// (`PoolStats::sync_ns`), against the same calls on an idle GPU.
#[test]
fn slab_sync_while_gpu_reads() {
    use ghostframe_client_core::TileSink;
    use ghostframe_client_gpu::pipelines::rgba_slab::{RgbaSlabPipeline, SlabWork};
    use ghostframe_client_gpu::upload_pool::UploadPool;

    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, W, H);
    let (pool, mut alloc) = UploadPool::with_slabs(&ctx, 2).expect("upload pool");
    let mut pipe = RgbaSlabPipeline::new(&ctx.device, &pool);
    let row = [0x7Au8; 128];
    let write = |alloc: &mut ghostframe_client_gpu::upload_pool::SlotAllocator, n: usize| {
        let mut work = Vec::new();
        for i in 0..n {
            let mut out = alloc.acquire().expect("slot");
            (0..32).for_each(|y| *out.row_mut(y) = row);
            let slot = alloc.commit(0, 0, 0);
            work.push((
                slot.slab(),
                SlabWork {
                    tile_x: i as u32 % 60,
                    tile_y: i as u32 / 60,
                    slot: slot.index(),
                },
            ));
        }
        alloc.end_cpu_writes();
        work
    };
    for rep in 0..12 {
        // Idle: a window on a slab nothing reads.
        let before = pool.stats();
        let first = write(&mut alloc, 900);
        let idle = pool.stats();
        // Busy: the GPU draws those 900 while the next 100 are written into
        // the SAME slab.
        let mut enc = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let batches = vec![(
            first[0].0,
            first.iter().map(|(_, w)| *w).collect::<Vec<_>>(),
        )];
        pipe.encode(&ctx.device, &ctx.queue, &mut enc, &fb, &pool, &batches);
        ctx.queue.submit([enc.finish()]);
        let t0 = Instant::now();
        let second = write(&mut alloc, 100);
        let wrote = Instant::now().saturating_duration_since(t0).as_secs_f64() * 1e3;
        let busy = pool.stats();
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        eprintln!(
            "rep={rep} sync idle: {} calls {:.3}ms | while gpu reads same slab: {} calls {:.3}ms, of which END {:.3}ms (100 tiles took {wrote:.2}ms)",
            idle.sync_calls - before.sync_calls,
            (idle.sync_ns - before.sync_ns) as f64 / 1e6,
            busy.sync_calls - idle.sync_calls,
            (busy.sync_ns - idle.sync_ns) as f64 / 1e6,
            (busy.sync_end_ns - idle.sync_end_ns) as f64 / 1e6,
        );
        // Fill the slab (900 + 100 + 24 = 1024) so the next rep gets another.
        let rest = write(&mut alloc, 24);
        let mut counts = vec![0u32; pool.slab_count()];
        for (slab, _) in first.iter().chain(&second).chain(&rest) {
            counts[*slab] += 1;
        }
        pool.retire_now(&counts);
    }
}

/// One frame of every codec at once, through the real `Renderer`: the case a
/// per-pipeline `submit` penalises, since each codec used to end in its own
/// glFlush and fence. Run by hand with `--ignored --nocapture`; under
/// `PAN_MESA_DEBUG=perf` the "Gallium flush" lines in the log count the
/// kernel submissions per frame.
#[test]
#[ignore = "hardware benchmark; run by hand"]
fn mixed_batch_flush_and_publish() {
    use ghostframe_client_core::{ClientConfig, ClientCore, Event, TileData, TileDelivery};
    use ghostframe_client_gpu::renderer::Renderer;

    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = Renderer::new(&ctx, W, H, 3, &[], false, false).expect("renderer");

    // CDF 5/3 passes come from the shared capture (tiles in the top rows).
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: false,
            supports_h264: false,
            h264_acked_ref: false,
            tile_delivery: TileDelivery::Payload,
            display: None,
        },
        0,
    );
    let mut cdf: Vec<Event> = Vec::new();
    for (i, d) in ghostframe_client_gpu::testdata::mixed_codec_capture()
        .iter()
        .enumerate()
    {
        cdf.extend(core.handle_datagram(d, i as u64 * 1_000));
    }

    let at = |i: u32, row0: u32| (i % 60, row0 + i / 60);
    let frame_events = |seq: u32| -> Vec<Event> {
        let mut evs = cdf.clone();
        evs.push(Event::PaletteUpdated {
            palette_id: 1,
            colors: (0..16u8).map(|c| [c * 15, 255 - c * 15, c, 255]).collect(),
        });
        for i in 0..240 {
            let (tile_x, tile_y) = at(i, 4);
            evs.push(Event::TilePayload {
                frame_seq: seq,
                tile_x: tile_x as u8,
                tile_y: tile_y as u8,
                generation: 0,
                data: TileData::Solid([i as u8, seq as u8, 7, 255]),
            });
            let (tile_x, tile_y) = at(i, 10);
            evs.push(Event::TilePayload {
                frame_seq: seq,
                tile_x: tile_x as u8,
                tile_y: tile_y as u8,
                generation: 0,
                data: TileData::PalRle {
                    palette_id: 1,
                    count: 16,
                    indices: vec![(i as u8) & 0x77; 512],
                    wire_flags: 0,
                },
            });
        }
        // Scattered TileReady (no two adjacent) and one 8x8 block.
        let rgba = vec![seq as u8; (TILE * TILE * 4) as usize];
        for y in 16..20u32 {
            for x in (0..60u32).filter(|x| (x + y) % 2 == 0) {
                evs.push(Event::TileReady {
                    frame_seq: seq,
                    tile_x: x as u8,
                    tile_y: y as u8,
                    rgba: rgba.clone(),
                });
            }
        }
        for y in 22..30u32 {
            for x in 0..8u32 {
                evs.push(Event::TileReady {
                    frame_seq: seq,
                    tile_x: x as u8,
                    tile_y: y as u8,
                    rgba: rgba.clone(),
                });
            }
        }
        evs
    };

    let mut run = |frames: u32, base: u32| -> f64 {
        let mut total = 0.0;
        for f in 0..frames {
            for ev in frame_events(base + f + 1) {
                r.apply_event(&ctx, ev).expect("gpu ok");
            }
            let t0 = Instant::now();
            let pf = r
                .flush_and_publish(&ctx)
                .expect("flush")
                .expect("a free export buffer");
            total += Instant::now().saturating_duration_since(t0).as_secs_f64() * 1e3;
            r.release(pf.frame_id);
        }
        total / frames as f64
    };
    run(10, 0); // warm-up: pipeline compile, buffer growth
    let ms = run(100, 10);
    eprintln!("\nmixed batch (240 solid, 240 palrle, 120+64 TileReady, 3 cdf53): flush+publish {ms:.2} ms/frame");
}

/// Scattered damage into an export buffer: the render-pass blit against the
/// compute copy of just the damaged tiles.
///
/// A render pass on a tiled GPU loads and stores every screen tile inside the
/// bounding box of its draws, so two corners cost the whole frame; the kernel
/// writes only the tiles it is given. Each round publishes into a one-buffer
/// ring whose history is exactly the marked tiles. `auto` is what production
/// runs: the kernel unless the damage fills most of its bounding box. "issue" is the publish call
/// (CPU), "+gpu" runs on until the GPU is idle.
#[test]
#[ignore = "hardware benchmark; run by hand"]
fn export_copy_blit_against_compute() {
    use ghostframe_client_gpu::ring::ExportRing;

    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, W, H);
    let (cols, rows) = tiles();
    let all = |keep: &dyn Fn(u32, u32) -> bool| -> Vec<(u32, u32)> {
        (0..rows)
            .flat_map(|ty| (0..cols).map(move |tx| (tx, ty)))
            .filter(|&(tx, ty)| keep(tx, ty))
            .collect()
    };
    let scenarios: Vec<(&str, Vec<(u32, u32)>)> = vec![
        ("two corners (2 tiles)", vec![(0, 0), (cols - 1, rows - 1)]),
        ("checkerboard (half)", all(&|x, y| (x + y) % 2 == 0)),
        ("one tile", vec![(cols / 2, rows / 2)]),
        ("one 8x8 block", all(&|x, y| x < 8 && y < 8)),
        ("every 4th tile", all(&|x, y| (x + y * cols) % 4 == 0)),
        ("full frame", all(&|_, _| true)),
    ];

    eprintln!();
    for (name, dirty) in &scenarios {
        for mode in ["blit", "compute", "auto"] {
            let mut ring = ExportRing::new(&ctx, W, H, 1, &[], false).expect("ring");
            match mode {
                "blit" => ring.force_render_pass_export(),
                "compute" => ring.force_compute_export(),
                _ => {}
            }
            assert_eq!(
                ring.uses_compute_copy(),
                mode != "blit",
                "mode did not take"
            );
            ring.mark_dirty_all();
            let f = ring.publish(&ctx, &fb).expect("first publish");
            ctx.device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("poll");
            ring.release(f.frame_id);

            let rounds = 60;
            let (mut issue, mut total) = (0.0f64, 0.0f64);
            for round in 0..rounds + 5 {
                for &(tx, ty) in dirty {
                    ring.mark_dirty(tx, ty);
                }
                let t0 = Instant::now();
                let f = ring.publish(&ctx, &fb).expect("publish");
                let submitted = Instant::now();
                ctx.device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("poll");
                let done = Instant::now();
                ring.release(f.frame_id);
                if round >= 5 {
                    issue += submitted.saturating_duration_since(t0).as_secs_f64() * 1e3;
                    total += done.saturating_duration_since(t0).as_secs_f64() * 1e3;
                }
            }
            eprintln!(
                "{name:<24} {mode:<8} issue {:>6.3}ms  +gpu {:>7.3}ms",
                issue / rounds as f64,
                total / rounds as f64
            );
        }
    }
}
