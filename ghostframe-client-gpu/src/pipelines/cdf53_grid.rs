// SPDX-License-Identifier: MIT OR Apache-2.0
//! The CDF 5/3 grid, as the native client uses it: `ghostframe-render`'s grid
//! plus the start-up probe, which needs a clock and a blocking wait.
//!
//! The probe is [`ghostframe_render::pipelines::cdf53_grid::self_test`] (is the
//! picture right?) followed by a calibration (what does a tile cost on this
//! GPU?). A browser runs the first and not the second.

pub use ghostframe_render::pipelines::cdf53_grid::*;

use crate::framebuffer::Framebuffer;
use ghostframe_render::readback::block_on_device;

/// A grid that passed [`probe`], with what the probe measured.
pub struct Probed {
    pub grid: Cdf53Grid,
    /// GPU microseconds per tile at a complete prefix (k = 14) on this device,
    /// from a 256-tile dispatch with the empty-submit overhead taken off.
    pub us_per_tile_k14: f64,
}

/// Build a grid, prove it works on this device, and measure what it costs.
///
/// Everything runs inside wgpu error scopes (validation, internal and
/// out-of-memory), so a pipeline that fails to build -- a shader the GL
/// backend cannot compile, say -- comes back as `Err` instead of reaching
/// wgpu's default uncaptured-error handler, which panics the render thread.
/// The caller then decodes on the CPU.
///
/// Blocks on the device; call it once, at start-up, on the render thread. The
/// grid it returns is the one that was tested: hand it to the renderer rather
/// than building another (the reconstruct shader is slow to compile on
/// panfrost).
pub fn probe(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Probed, String> {
    probe_with(device, queue, Cdf53Grid::new)
}

/// [`probe`] with the grid made by `make` (a test that must see it fail).
pub fn probe_with(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    make: impl FnOnce(&wgpu::Device) -> Cdf53Grid,
) -> Result<Probed, String> {
    let scopes = ErrorScopes::push(device);
    let result = (|| {
        let mut grid = make(device);
        self_test_with(device, queue, &mut grid)?;
        let us_per_tile_k14 = calibrate(device, queue, &mut grid)?;
        Ok(Probed {
            grid,
            us_per_tile_k14,
        })
    })();
    if let Some(e) = block_on_device(device, scopes.finish()) {
        return Err(e);
    }
    result
}

/// [`self_test`], blocking.
pub fn self_test_with(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    grid: &mut Cdf53Grid,
) -> Result<(), String> {
    block_on_device(device, self_test(device, queue, grid))
}

/// Tiles in the calibration dispatch: enough that the fixed cost of a submit
/// is a small part of it (at 64 tiles it inflated the per-tile figure by 40%
/// on a Mali-T860), few enough to take ~15 ms there.
const CALIBRATION_TILES: u32 = 256;

/// GPU microseconds per complete tile on this device: the wall time of
/// submit + wait for a [`CALIBRATION_TILES`]-tile frame of known content,
/// minus that of an empty submit, median of five after a warm-up. Wall time
/// includes the driver's own overhead, which is what a flush pays too.
fn calibrate(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    grid: &mut Cdf53Grid,
) -> Result<f64, String> {
    use std::time::Instant;
    let tiles = self_test_tiles();
    let fixture = &tiles[0];
    grid.resize(device, 16, 16).map_err(|e| e.to_string())?;
    let fb = Framebuffer::new(device, 16 * 32, 16 * 32);
    let wait = || device.poll(wgpu::PollType::wait_indefinitely());
    let timed = |f: &mut dyn FnMut() -> Result<(), String>| -> Result<f64, String> {
        let t = Instant::now();
        f()?;
        wait().map_err(|e| e.to_string())?;
        Ok(Instant::now().duration_since(t).as_secs_f64() * 1e6)
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    };
    let mut empty = || {
        let enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        queue.submit(std::iter::once(enc.finish()));
        Ok(())
    };
    let mut full = || {
        grid.begin_frame();
        for i in 0..CALIBRATION_TILES {
            for (p, plane) in fixture.planes.iter().enumerate() {
                if let Some(plane) = plane {
                    grid.push_plane(i, p as u8, plane);
                }
            }
            grid.push_tile(i, fixture.present as u32, 14);
        }
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-cdf53-calibration"),
        });
        if !grid.record(device, queue, &mut enc, &fb) {
            return Err("calibration recorded nothing".to_string());
        }
        queue.submit(std::iter::once(enc.finish()));
        Ok(())
    };
    timed(&mut full)?; // warm-up: first use compiles and binds
    let mut base = Vec::new();
    let mut work = Vec::new();
    for _ in 0..5 {
        base.push(timed(&mut empty)?);
        work.push(timed(&mut full)?);
    }
    let per_tile = (median(work) - median(base)) / CALIBRATION_TILES as f64;
    Ok(per_tile.clamp(0.5, 500.0))
}
