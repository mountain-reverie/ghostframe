//! Damage-driven encoding, library side (PR 6): a frame told which tile rows
//! changed uploads and compares only those rows, and must leave exactly what a
//! full compare would have left.
//!
//! The oracle: for any `Tiles(S)` where `S` holds every tile that really
//! changed, the dirty list, the NV12 buffer, the analysis of the dirty tiles
//! and the committed reference equal the `Unknown` run's, frame after frame.
//! Both runs are on the GPU (the CPU path has no SAD threshold).
#![allow(
    clippy::disallowed_methods,
    reason = "wall-clock measurement of real GPU time in the #[ignore]d report"
)]

use super::super::pipeline_builder::alloc_host_buffer_mapped;
use super::*;
use crate::frame_damage::{FrameDamage, TileBits};
use proptest::prelude::any;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

const NEEDS_GPU: &str = "needs a Vulkan GPU";

/// What one frame left behind.
#[derive(Debug, PartialEq)]
struct Snap {
    dirty: Vec<u32>,
    nv12: Vec<u8>,
    /// The analysis of each dirty tile, colours sorted (the type has no `Eq`).
    analysis: Vec<String>,
    /// The committed per-tile reference, tightly packed BGRA.
    reference: Vec<u8>,
}

fn step(gpu: &mut GpuFrameProcessor, px: &[u8], w: u32, h: u32, d: &FrameDamage) -> Snap {
    let a = unsafe { gpu.process_frame_inner_from_pixels_damaged(px, w, h, w * 4, d) }
        .expect("process_frame");
    let uv = (a.nv12_uv_stride * h.div_ceil(2)) as usize;
    let nv12 =
        unsafe { std::slice::from_raw_parts(a.nv12_data, a.nv12_uv_offset as usize + uv).to_vec() };
    let ta = a.tile_analysis_slice();
    let analysis = a
        .dirty_tiles
        .iter()
        .map(|&i| {
            let t = ta[i as usize];
            // Colours come back in slot order, and are undefined past 16.
            let mut colors = t.colors[..(t.count as usize).min(16)].to_vec();
            if t.count > 16 {
                colors.clear();
            }
            colors.sort_unstable();
            format!("{} {} {colors:?}", t.count, t.edge_density_thou)
        })
        .collect();
    let dirty = a.dirty_tiles.clone();
    let reference = unsafe { read_reference(gpu, w, h) };
    Snap {
        dirty,
        nv12,
        analysis,
        reference,
    }
}

/// The committed reference image, read back.
unsafe fn read_reference(gpu: &GpuFrameProcessor, w: u32, h: u32) -> Vec<u8> {
    let Some(prev) = gpu.prev_image.as_ref() else {
        return Vec::new();
    };
    let props = gpu
        .instance
        .get_physical_device_memory_properties(gpu.physical_device);
    let size = u64::from(w) * u64::from(h) * 4;
    let (buf, mem, ptr) = alloc_host_buffer_mapped::<u8>(
        &gpu.device,
        &props,
        size,
        vk::BufferUsageFlags::TRANSFER_DST,
        "reference readback",
    )
    .expect("readback buffer");
    let cmd = gpu.begin_frame_cmd().expect("cmd");
    gpu.device
        .begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .unwrap();
    let region = vk::BufferImageCopy::default()
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_extent(vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        });
    gpu.device
        .cmd_copy_image_to_buffer(cmd, prev.image, vk::ImageLayout::GENERAL, buf, &[region]);
    let barrier = vk::MemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::HOST_READ);
    gpu.device.cmd_pipeline_barrier(
        cmd,
        vk::PipelineStageFlags::TRANSFER,
        vk::PipelineStageFlags::HOST,
        vk::DependencyFlags::empty(),
        &[barrier],
        &[],
        &[],
    );
    gpu.device.end_command_buffer(cmd).unwrap();
    gpu.submit_frame_cmd_and_wait(cmd).expect("submit");
    let out = std::slice::from_raw_parts(ptr, size as usize).to_vec();
    gpu.device.destroy_buffer(buf, None);
    gpu.device.unmap_memory(mem);
    gpu.device.free_memory(mem, None);
    out
}

// -- frames -----------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn cols_rows(w: u32, h: u32) -> (u32, u32) {
    (w.div_ceil(32), h.div_ceil(32))
}

/// A frame whose tiles alternate between opaque noise (over the palette
/// limit) and four flat colours (a palette tile), so the analysis differs.
fn noise_frame(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let (cols, _) = cols_rows(w, h);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let pal = [[10u8, 20, 30], [200, 100, 50], [0, 255, 0], [90, 90, 255]];
    for y in 0..h {
        for x in 0..w {
            let t = (y / 32) * cols + x / 32;
            let o = ((y * w + x) * 4) as usize;
            if t % 2 == 0 {
                let v = r.next();
                px[o..o + 3].copy_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8]);
            } else {
                px[o..o + 3].copy_from_slice(&pal[((x / 8 + y / 8) % 4) as usize]);
            }
            px[o + 3] = 255;
        }
    }
    px
}

/// How a tile changes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Change {
    /// Every pixel replaced: SAD far over the threshold.
    Big,
    /// Five pixels nudged by 3 in R: SAD about 15, under the threshold, so the
    /// tile drifts against its reference instead of being committed.
    Small,
}

fn change_tile(px: &mut [u8], w: u32, h: u32, tile: u32, how: Change, r: &mut Rng) {
    let (cols, _) = cols_rows(w, h);
    let (tx, ty) = (tile % cols, tile / cols);
    let (x0, y0) = (tx * 32, ty * 32);
    let (x1, y1) = ((x0 + 32).min(w), (y0 + 32).min(h));
    match how {
        Change::Big => {
            for y in y0..y1 {
                for x in x0..x1 {
                    let o = ((y * w + x) * 4) as usize;
                    let v = r.next();
                    px[o..o + 3].copy_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8]);
                }
            }
        }
        Change::Small => {
            for _ in 0..5 {
                let x = x0 + r.below(u64::from(x1 - x0)) as u32;
                let y = y0 + r.below(u64::from(y1 - y0)) as u32;
                let o = ((y * w + x) * 4 + 2) as usize;
                px[o] = px[o].wrapping_add(3);
            }
        }
    }
}

fn tiles_damage(w: u32, h: u32, hint: &[u32], verify: &[u32]) -> FrameDamage {
    let mk = |v: &[u32]| {
        let mut b = TileBits::new(w, h);
        for &i in v {
            b.set(i as usize);
        }
        b
    };
    FrameDamage::Tiles {
        hint: mk(hint),
        verify: mk(verify),
    }
}

/// Run the same frames through an `Unknown` processor and a `Tiles` one.
/// `frames[i]` is `(pixels, damage for frame i)`; frame 0 is always whole.
fn oracle(
    a: &mut GpuFrameProcessor,
    b: &mut GpuFrameProcessor,
    w: u32,
    h: u32,
    frames: &[(Vec<u8>, FrameDamage)],
) -> Result<usize, String> {
    oracle_with(a, b, w, h, frames, None)
}

/// [`oracle`], dropping both baselines before frame `invalidate_at`.
fn oracle_with(
    a: &mut GpuFrameProcessor,
    b: &mut GpuFrameProcessor,
    w: u32,
    h: u32,
    frames: &[(Vec<u8>, FrameDamage)],
    invalidate_at: Option<usize>,
) -> Result<usize, String> {
    // Frames that took the partial path: fewer tile rows compared than exist.
    let mut partial = 0;
    a.invalidate_baseline(0);
    b.invalidate_baseline(0);
    for (i, (px, damage)) in frames.iter().enumerate() {
        if invalidate_at == Some(i) {
            a.invalidate_baseline(0);
            b.invalidate_baseline(0);
        }
        let d = if i == 0 {
            &FrameDamage::Unknown
        } else {
            damage
        };
        let want = step(a, px, w, h, &FrameDamage::Unknown);
        let got = step(b, px, w, h, d);
        if i > 0 && b.probe.dispatched_tile_rows < cols_rows(w, h).1 {
            partial += 1;
        }
        if want.dirty != got.dirty {
            return Err(format!(
                "frame {i}: dirty {:?} != Unknown's {:?}",
                got.dirty, want.dirty
            ));
        }
        if want.reference != got.reference {
            return Err(format!("frame {i}: committed reference differs"));
        }
        if want.nv12 != got.nv12 {
            return Err(format!("frame {i}: NV12 differs"));
        }
        if want.analysis != got.analysis {
            return Err(format!("frame {i}: analysis of the dirty tiles differs"));
        }
    }
    Ok(partial)
}

const SIZES: [(u32, u32); 4] = [(96, 64), (100, 70), (160, 128), (192, 160)];

/// A random sequence in which every frame's damage is a superset of what
/// changed (`extra` adds tiles that did not).
fn sequence(
    w: u32,
    h: u32,
    seed: u64,
    n_frames: usize,
    extra: bool,
) -> Vec<(Vec<u8>, FrameDamage)> {
    let mut r = Rng(seed | 1);
    let (cols, rows) = cols_rows(w, h);
    let tiles = cols * rows;
    let mut px = noise_frame(w, h, seed ^ 0x9e37_79b9);
    let mut out = vec![(px.clone(), FrameDamage::Unknown)];
    for _ in 1..n_frames {
        let mut changed = Vec::new();
        for t in 0..tiles {
            match r.below(6) {
                0 => {
                    change_tile(&mut px, w, h, t, Change::Big, &mut r);
                    changed.push(t);
                }
                1 => {
                    change_tile(&mut px, w, h, t, Change::Small, &mut r);
                    changed.push(t);
                }
                _ => {}
            }
        }
        let (mut hint, mut verify) = (Vec::new(), Vec::new());
        for t in 0..tiles {
            let named = changed.contains(&t) || (extra && r.below(8) == 0);
            if named {
                if r.below(3) == 0 {
                    verify.push(t);
                } else {
                    hint.push(t);
                }
            }
        }
        out.push((px.clone(), tiles_damage(w, h, &hint, &verify)));
    }
    out
}

fn pair() -> (GpuFrameProcessor, GpuFrameProcessor) {
    (
        GpuFrameProcessor::new(256).expect(NEEDS_GPU),
        GpuFrameProcessor::new(256).expect(NEEDS_GPU),
    )
}

/// Over random frame sequences whose damage is a superset of the change, a
/// damaged run equals the whole-frame run: dirty list, NV12, analysis and
/// the committed reference, every frame.
#[test]
fn oracle_any_superset_of_the_changed_tiles_equals_the_unknown_run() {
    let (a, b) = pair();
    let (a, b) = (std::cell::RefCell::new(a), std::cell::RefCell::new(b));
    let mut runner = TestRunner::new(Config {
        cases: 40,
        ..Config::default()
    });
    let partial = std::cell::Cell::new(0usize);
    runner
        .run(
            &(0usize..SIZES.len(), any::<u64>(), 2usize..5, any::<bool>()),
            |(si, seed, n, extra)| {
                let (w, h) = SIZES[si];
                let frames = sequence(w, h, seed, n, extra);
                let p = oracle(&mut a.borrow_mut(), &mut b.borrow_mut(), w, h, &frames)
                    .map_err(TestCaseError::fail)?;
                partial.set(partial.get() + p);
                Ok(())
            },
        )
        .unwrap();
    // The property is vacuous if the damaged run rarely took the partial path.
    assert!(
        partial.get() >= 20,
        "only {} frames compared fewer rows than the whole frame",
        partial.get()
    );
}

/// The mutation of the oracle's precondition: a tile that changed but is in
/// no named row is missed, and the oracle says so. Without this the oracle
/// could pass for any input.
#[test]
fn oracle_fails_when_a_changed_tile_is_omitted() {
    let (mut a, mut b) = pair();
    let (w, h) = (160u32, 128u32); // 5 x 4 tiles
    let mut r = Rng(7);
    let f0 = noise_frame(w, h, 1);
    let mut f1 = f0.clone();
    change_tile(&mut f1, w, h, 12, Change::Big, &mut r); // tile (2, 2)
    change_tile(&mut f1, w, h, 1, Change::Big, &mut r); // tile (1, 0)

    let named_both = tiles_damage(w, h, &[1, 12], &[]);
    let partial = oracle(
        &mut a,
        &mut b,
        w,
        h,
        &[(f0.clone(), FrameDamage::Unknown), (f1.clone(), named_both)],
    )
    .expect("naming every changed tile is the oracle's precondition");
    assert_eq!(partial, 1, "rows 0 and 2 of 4 are compared");

    let omitted = tiles_damage(w, h, &[1], &[]); // tile 12 changed, row 2 unnamed
    let err = oracle(
        &mut a,
        &mut b,
        w,
        h,
        &[(f0, FrameDamage::Unknown), (f1, omitted)],
    )
    .expect_err("an omitted changed tile must break the oracle");
    assert!(err.contains("dirty"), "{err}");
}

/// The authority rule: every tile in an uploaded band is a SAD candidate. A
/// tile that changed and is not named, in a row that is, is still sent.
#[test]
fn a_changed_unnamed_tile_inside_a_band_is_still_sent() {
    let (w, h) = (160u32, 128u32); // 5 x 4 tiles, row 1 = tiles 5..10
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let mut r = Rng(3);
    let f0 = noise_frame(w, h, 2);
    step(&mut gpu, &f0, w, h, &FrameDamage::Unknown);
    let mut f1 = f0.clone();
    change_tile(&mut f1, w, h, 6, Change::Big, &mut r); // named
    change_tile(&mut f1, w, h, 8, Change::Big, &mut r); // same row, NOT named
    change_tile(&mut f1, w, h, 17, Change::Big, &mut r); // another row, NOT named
    let got = step(&mut gpu, &f1, w, h, &tiles_damage(w, h, &[6], &[]));
    assert_eq!(
        got.dirty,
        vec![6, 8],
        "tile 8 shares a band with the hint and must be sent; tile 17 is in no band"
    );
    assert_eq!(
        gpu.probe.dispatched_tile_rows, 1,
        "only the band is compared"
    );

    // Tile 17 was not compared, so the reference still holds its old pixels
    // and the next frame that reaches its row reports it: the sweep's job.
    let got = step(&mut gpu, &f1, w, h, &tiles_damage(w, h, &[], &[17]));
    assert_eq!(got.dirty, vec![17]);
}

/// The uploaded and dispatched rows are exactly the tile rows holding a named
/// tile, across damaged fractions; nothing else is copied or compared.
#[test]
fn uploaded_and_compared_rows_are_the_named_rows() {
    let (w, h) = (192u32, 160u32); // 6 x 5 tiles
    let (cols, rows) = cols_rows(w, h);
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let f0 = noise_frame(w, h, 5);
    step(&mut gpu, &f0, w, h, &FrameDamage::Unknown);
    let mut last = 0;
    for n in 0..=rows {
        // Name one tile in each of the first `n` rows.
        let hint: Vec<u32> = (0..n).map(|r| r * cols + 1).collect();
        step(&mut gpu, &f0, w, h, &tiles_damage(w, h, &hint, &[]));
        let p = gpu.probe;
        assert_eq!(p.dispatched_tile_rows, n, "{n} rows named");
        assert_eq!(p.uploaded_rows, (n * 32).min(h), "{n} rows named");
        assert!(p.uploaded_rows >= last, "monotone in the damaged rows");
        last = p.uploaded_rows;
    }
    // Unknown is the whole frame.
    step(&mut gpu, &f0, w, h, &FrameDamage::Unknown);
    assert_eq!(gpu.probe.uploaded_rows, h);
    assert_eq!(gpu.probe.dispatched_tile_rows, rows);
}

/// Damage is ignored until a whole frame has gone through: after a dropped
/// frame, a failed one or a resize the named rows are not enough.
#[test]
fn damage_is_ignored_until_a_whole_frame_has_been_processed() {
    let (w, h) = (160u32, 128u32);
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let mut r = Rng(11);
    let f0 = noise_frame(w, h, 9);
    step(&mut gpu, &f0, w, h, &FrameDamage::Unknown);
    let mut f1 = f0.clone();
    change_tile(&mut f1, w, h, 12, Change::Big, &mut r);

    // The frame carrying tile 12's change never reached the processor.
    gpu.forget_damage();
    let mut f2 = f1.clone();
    change_tile(&mut f2, w, h, 1, Change::Big, &mut r);
    let got = step(&mut gpu, &f2, w, h, &tiles_damage(w, h, &[1], &[]));
    assert_eq!(got.dirty, vec![1, 12], "compared whole after forget_damage");
    assert_eq!(gpu.probe.uploaded_rows, h);

    // And the very next frame trusts damage again.
    let got = step(&mut gpu, &f2, w, h, &tiles_damage(w, h, &[1], &[]));
    assert!(got.dirty.is_empty());
    assert_eq!(gpu.probe.dispatched_tile_rows, 1);

    // A resize and back: the upload image is rebuilt, so damage cannot be
    // trusted for the first frame at each size.
    let (w2, h2) = (96u32, 128u32);
    let g1 = noise_frame(w2, h2, 4);
    let got = step(&mut gpu, &g1, w2, h2, &tiles_damage(w2, h2, &[], &[]));
    let (c2, r2) = cols_rows(w2, h2);
    assert_eq!(got.dirty.len() as u32, c2 * r2, "first frame at a new size");
    assert_eq!(gpu.probe.uploaded_rows, h2);
    let mut f3 = f2.clone();
    change_tile(&mut f3, w, h, 7, Change::Big, &mut r);
    let got = step(&mut gpu, &f3, w, h, &tiles_damage(w, h, &[], &[]));
    let (c1, r1) = cols_rows(w, h);
    assert_eq!(got.dirty.len() as u32, c1 * r1, "back at the old size");
    assert_eq!(gpu.probe.uploaded_rows, h);

    // A frame the processor rejects (one byte short) leaves nothing to trust.
    let mut f4 = f3.clone();
    change_tile(&mut f4, w, h, 3, Change::Big, &mut r);
    let short = &f4[..f4.len() - 1];
    assert!(unsafe {
        gpu.process_frame_inner_from_pixels_damaged(short, w, h, w * 4, &FrameDamage::Unknown)
    }
    .is_err());
    let mut f5 = f4.clone();
    change_tile(&mut f5, w, h, 9, Change::Big, &mut r);
    let got = step(&mut gpu, &f5, w, h, &tiles_damage(w, h, &[9], &[]));
    assert_eq!(
        got.dirty,
        vec![3, 9],
        "compared whole after a rejected frame"
    );

    // A frame with damage for another frame size says nothing.
    let got = step(&mut gpu, &f5, w, h, &tiles_damage(64, 64, &[0], &[]));
    assert!(got.dirty.is_empty());
    assert_eq!(gpu.probe.uploaded_rows, h, "mis-sized damage uploads whole");
}

/// Upload and GPU time against the damaged fraction, `Unknown` against
/// `Tiles`, in one process. A measurement, not a gate (the gate is the row
/// counts above).
/// `gf-gpu-run cargo test -p ghostframe-lib --lib damage_report -- --ignored --nocapture`
#[test]
#[ignore]
fn damage_report() {
    for (label, w, h) in [("1080p", 1920u32, 1088u32), ("4K", 3840, 2160)] {
        let (cols, rows) = cols_rows(w, h);
        let mut gpu = GpuFrameProcessor::new(cols * rows + 8).expect(NEEDS_GPU);
        // (scene, damaged tile rows, changed tiles per damaged row)
        let mut scenes: Vec<(String, u32, u32)> = vec![("typing".into(), 1, 2)];
        for pct in [0u32, 1, 5, 25, 100] {
            scenes.push((format!("scroll{pct}%"), (rows * pct).div_ceil(100), cols));
        }
        for (scene, nrows, per_row) in scenes {
            let base = noise_like(w, h, 0);
            let frames: Vec<Vec<u8>> = (1..=14u64)
                .map(|n| {
                    let mut f = base.clone();
                    // Scroll or type: the damaged rows get new content each tick.
                    let mut r = Rng(n * 7919);
                    for ty in 0..nrows {
                        for tx in 0..per_row {
                            change_tile(&mut f, w, h, ty * cols + tx, Change::Big, &mut r);
                        }
                    }
                    f
                })
                .collect();
            let hint: Vec<u32> = (0..nrows).map(|r| r * cols).collect();
            for (mode, damage) in [
                ("unknown", FrameDamage::Unknown),
                ("tiles", tiles_damage(w, h, &hint, &[])),
            ] {
                // Seed the baseline, then alternate the base and a changed frame
                // so every measured frame changes the same rows.
                unsafe { gpu.process_frame_inner_from_pixels(&base, w, h, w * 4) }.unwrap();
                let (mut up, mut gp) = (Vec::new(), Vec::new());
                for f in &frames {
                    unsafe { gpu.process_frame_inner_from_pixels_damaged(f, w, h, w * 4, &damage) }
                        .unwrap();
                    up.push(gpu.probe.upload_us);
                    gp.push(gpu.probe.gpu_us);
                    // Back to the base so the next frame differs in the same rows.
                    unsafe {
                        gpu.process_frame_inner_from_pixels_damaged(&base, w, h, w * 4, &damage)
                    }
                    .unwrap();
                }
                up.drain(..2);
                gp.drain(..2);
                up.sort_unstable();
                gp.sort_unstable();
                println!(
                    "DMG {label} {scene} mode={mode} rows={nrows} upload_us={} gpu_us={}",
                    up[up.len() / 2],
                    gp[gp.len() / 2]
                );
            }
        }
    }
}

/// Flat mid-grey with a pattern: cheap to build at 4K, unlike noise.
fn noise_like(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let v = ((x * 3 + y + seed as u32) % 200) as u8;
            let o = ((y * w + x) * 4) as usize;
            px[o..o + 4].copy_from_slice(&[v, v ^ 0x55, v.wrapping_mul(3), 255]);
        }
    }
    px
}

/// 17 separate runs of tile rows at 1080p exceed the 16 dispatches a stage
/// makes, so `merge_runs` joins a pair and the gap row is compared too.
#[test]
fn oracle_more_runs_than_dispatches_are_merged_and_still_equal() {
    let (w, h) = (1920u32, 1088u32);
    let (cols, rows) = cols_rows(w, h);
    let mut a = GpuFrameProcessor::new(cols * rows + 8).expect(NEEDS_GPU);
    let mut b = GpuFrameProcessor::new(cols * rows + 8).expect(NEEDS_GPU);
    let mut r = Rng(21);
    let f0 = noise_like(w, h, 1);
    let mut f1 = f0.clone();
    let mut named = Vec::new();
    for ty in (0..rows).step_by(2) {
        let t = ty * cols + 5;
        change_tile(&mut f1, w, h, t, Change::Big, &mut r);
        named.push(t);
    }
    assert_eq!(named.len(), 17);
    let partial = oracle(
        &mut a,
        &mut b,
        w,
        h,
        &[
            (f0, FrameDamage::Unknown),
            (f1, tiles_damage(w, h, &named, &[])),
        ],
    )
    .unwrap();
    assert_eq!(partial, 1);
    let dispatched = b.probe.dispatched_tile_rows;
    assert!(
        (18..rows).contains(&dispatched),
        "17 rows named, 16 runs dispatched: {dispatched} rows compared"
    );
}

/// Dropping the baseline while damage is trusted (a session reset) leaves the
/// next damaged frame comparing everything, on both paths alike.
#[test]
fn oracle_holds_across_a_baseline_invalidation() {
    let (mut a, mut b) = pair();
    let (w, h) = (160u32, 128u32);
    for seed in [3u64, 77] {
        let frames = sequence(w, h, seed, 5, true);
        oracle_with(&mut a, &mut b, w, h, &frames, Some(3)).unwrap();
    }
}

// -- Content::Unchanged (PR 7) ------------------------------------------------

/// The escalation forward's coefficients for `n` candidates, read back.
fn escalation_coeffs(gpu: &GpuFrameProcessor, n: usize) -> Vec<i32> {
    if n == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(gpu.cdf53_escalation_coefficients_ptr, n * 3072).to_vec() }
}

/// An unchanged tick leaves what a full compare of identical pixels leaves:
/// no dirty tile, the same NV12, analysis and reference, and an escalation
/// forward that reads the screen (the upload image), not the SAD reference.
#[test]
fn an_unchanged_tick_equals_a_full_compare_of_identical_pixels() {
    let (w, h) = (160u32, 128u32);
    let (mut full, mut unch) = pair();
    let mut r = Rng(77);
    let mut px = noise_frame(w, h, 21);
    let candidates = [3u32, 7, 12];
    for gpu in [&mut full, &mut unch] {
        gpu.invalidate_baseline(0);
    }
    step(&mut full, &px, w, h, &FrameDamage::Unknown);
    step(&mut unch, &px, w, h, &FrameDamage::Unknown);
    let mut esc_nonzero = 0;
    for round in 0..6 {
        // A change, then idle ticks. The change on tile 5 is sub-threshold
        // after round 2 (one byte), so the reference and the screen differ.
        if round < 3 {
            change_tile(&mut px, w, h, 5 + round, Change::Big, &mut r);
        } else {
            let o = (32 * w * 4 + 4) as usize;
            px[o] = px[o].wrapping_add(1);
        }
        let d = FrameDamage::Unknown;
        assert_eq!(
            step(&mut full, &px, w, h, &d),
            step(&mut unch, &px, w, h, &d),
            "round {round}: changed frame"
        );
        for tick in 0..3 {
            full.set_escalation_candidates(&candidates);
            unch.set_escalation_candidates(&candidates);
            let want = step(&mut full, &px, w, h, &FrameDamage::Unknown);
            let a = unsafe { unch.process_frame_inner_unchanged(w, h) }.expect("unchanged tick");
            let got = {
                // Same readback as `step`, from the analysis just made.
                let uv = (a.nv12_uv_stride * h.div_ceil(2)) as usize;
                let nv12 = unsafe {
                    std::slice::from_raw_parts(a.nv12_data, a.nv12_uv_offset as usize + uv).to_vec()
                };
                (a.dirty_tiles.clone(), nv12)
            };
            assert!(
                got.0.is_empty(),
                "round {round} tick {tick}: dirty {:?}",
                got.0
            );
            assert_eq!(want.dirty, got.0);
            assert_eq!(want.nv12, got.1, "round {round} tick {tick}: NV12");
            assert_eq!(
                want.reference,
                unsafe { read_reference(&unch, w, h) },
                "round {round} tick {tick}: reference"
            );
            let (we, ge) = (
                escalation_coeffs(&full, candidates.len()),
                escalation_coeffs(&unch, candidates.len()),
            );
            assert_eq!(we, ge, "round {round} tick {tick}: escalation forward");
            esc_nonzero += usize::from(ge.iter().any(|&v| v != 0));
            // Nothing was uploaded or compared.
            assert_eq!(unch.probe.uploaded_rows, 0);
            assert_eq!(unch.probe.dispatched_tile_rows, 0);
        }
    }
    assert!(
        esc_nonzero > 0,
        "the escalation forward never produced coefficients"
    );
}

/// An unchanged tick that the upload image cannot stand in for is refused and
/// leaves the next pixels frame to compare whole.
#[test]
fn an_unchanged_tick_without_a_consistent_upload_image_is_refused() {
    let (w, h) = (160u32, 128u32);
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    // Nothing uploaded yet.
    assert!(unsafe { gpu.process_frame_inner_unchanged(w, h) }.is_err());
    let mut r = Rng(5);
    let f0 = noise_frame(w, h, 3);
    step(&mut gpu, &f0, w, h, &FrameDamage::Unknown);
    // Another size.
    assert!(unsafe { gpu.process_frame_inner_unchanged(96, 128) }.is_err());
    // The refusal cleared the baseline, so damage is not trusted next.
    let mut f1 = f0.clone();
    change_tile(&mut f1, w, h, 9, Change::Big, &mut r);
    let got = step(&mut gpu, &f1, w, h, &tiles_damage(w, h, &[], &[]));
    assert_eq!(got.dirty, vec![9], "compared whole after a refused tick");
    assert_eq!(gpu.probe.uploaded_rows, h);
    // After forget_damage the upload image is not known to match the screen.
    gpu.forget_damage();
    assert!(unsafe { gpu.process_frame_inner_unchanged(w, h) }.is_err());
}

/// During a session-reset cushion the unchanged tick has no reference and
/// reports every tile dirty, from the upload image; the read-back is that
/// image.
#[test]
fn an_unchanged_tick_in_the_reset_cushion_reports_every_tile_from_the_upload_image() {
    let (w, h) = (160u32, 128u32);
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let f0 = noise_frame(w, h, 8);
    step(&mut gpu, &f0, w, h, &FrameDamage::Unknown);
    gpu.invalidate_baseline(2);
    let a = unsafe { gpu.process_frame_inner_unchanged(w, h) }.expect("unchanged tick");
    let (c, rws) = cols_rows(w, h);
    assert_eq!(a.dirty_tiles.len() as u32, c * rws);
    assert_eq!(
        gpu.read_upload_pixels(w, h, w * 4).as_deref(),
        Some(&f0[..])
    );
}
