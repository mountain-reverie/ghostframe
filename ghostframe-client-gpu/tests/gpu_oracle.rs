//! The two-decoder oracle.
//!
//! `ClientCore` can deliver the same datagrams CPU-decoded
//! (TileDelivery::Decoded -> Event::TileReady) or raw for a GPU decoder
//! (TileDelivery::Payload -> Event::TilePayload). Feeding one capture
//! through both and comparing pixels is a direct test that the Rust
//! decoder and the real WGSL agree.
//!
//! Requires a GPU; NOT named in any CI workflow.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, TileDelivery};
use ghostframe_client_gpu::{renderer::Renderer, testdata, wgpu_ctx::WgpuContext};
use std::collections::HashMap;

fn drain(datagrams: &[Vec<u8>], delivery: TileDelivery) -> Vec<Event> {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: false,
            supports_h264: false,
            tile_delivery: delivery,
        },
        0,
    );
    let mut events = Vec::new();
    for (i, d) in datagrams.iter().enumerate() {
        events.extend(core.handle_datagram(d, i as u64 * 1_000));
    }
    events
}

#[test]
fn gpu_decode_matches_cpu_decode_for_solid_palrle_and_cdf53() {
    let capture = testdata::mixed_codec_capture();

    // CPU reference.
    let mut expected: HashMap<(u8, u8), Vec<u8>> = HashMap::new();
    for ev in drain(&capture, TileDelivery::Decoded) {
        if let Event::TileReady {
            tile_x,
            tile_y,
            rgba,
            ..
        } = ev
        {
            expected.insert((tile_x, tile_y), rgba);
        }
    }
    assert!(
        !expected.is_empty(),
        "capture produced no CPU-decoded tiles"
    );
    assert_eq!(
        expected.len(),
        3,
        "expected exactly 3 CPU-decoded tiles (Solid, PalRle, Cdf53), got {}",
        expected.len()
    );

    // GPU path, same capture.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut renderer = Renderer::new(&ctx, 64, 64, 3, &[], false, false).expect("renderer");
    for ev in drain(&capture, TileDelivery::Payload) {
        renderer.apply_event(&ctx, &ev);
    }
    renderer.flush(&ctx);
    let fb = renderer.debug_read_framebuffer(&ctx);

    let mut compared = 0usize;
    for ((tx, ty), cpu_rgba) in &expected {
        for i in 0..1024usize {
            let px = *tx as u32 * 32 + (i as u32 % 32);
            let py = *ty as u32 * 32 + (i as u32 / 32);
            let o = ((py * 64 + px) * 4) as usize;
            let gpu = &fb[o..o + 4];
            let cpu = &cpu_rgba[i * 4..i * 4 + 4];
            // EXACT equality, no tolerance.
            //
            // CDF 5/3 is an integer-reversible lifting transform, and the
            // inverse shaders (cdf53_inverse_l1/l2/l3) contain no floating
            // point at all -- only i32. The sole f32 in the chain is
            // cdf53_inverse_l1_pass2's final `/255.0` for textureStore into
            // an rgba8unorm target, which round-trips integers 0..255
            // exactly.
            //
            // Both decoders also apply the same midpoint formula for an
            // incomplete pass set, so they must agree even mid-refinement.
            // A tolerance here would hide precisely the systematic drift
            // this oracle exists to detect: the known sparse-K bug is worth
            // ~16/255, but a smaller constant offset would slip under any
            // slack we allowed.
            assert_eq!(
                gpu, cpu,
                "tile ({tx},{ty}) pixel {i}: gpu {gpu:?} != cpu {cpu:?}"
            );
            compared += 1;
        }
    }
    // Guard against the test silently comparing nothing.
    assert!(compared >= 3 * 1024, "only compared {compared} pixels");
}
