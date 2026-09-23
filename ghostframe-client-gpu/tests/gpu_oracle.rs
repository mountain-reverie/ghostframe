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
    let mut renderer = Renderer::new(&ctx, 64, 64).expect("renderer");
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
            for c in 0..4 {
                assert!(
                    (gpu[c] as i32 - cpu[c] as i32).abs() <= 2,
                    "tile ({tx},{ty}) pixel {i} channel {c}: gpu {} cpu {}",
                    gpu[c],
                    cpu[c]
                );
            }
            compared += 1;
        }
    }
    // Guard against the test silently comparing nothing.
    assert!(compared >= 3 * 1024, "only compared {compared} pixels");
}
