//! A dirty tile no tile encoder can carry goes out as CDF 5/3, never Raw, and
//! decodes to exactly the pixels it was sent from.
//!
//! These tests run the real `dispatch_dirty_tiles_via_scheduler` with the GPU
//! policy. Without a GPU processor the fallback transform runs on the CPU
//! (`#[cfg(test)]` scaffolding only); the GPU transform itself is pinned
//! against that same CPU transform by `gpu_pipeline::fallback_tests`.

use super::*;
use crate::encoder::cdf53;
use crate::tile::CodecState;
use crate::transport::protocol::Codec;
use tokio::net::UnixStream;

const W: u32 = 128;

fn bridge() -> IoBridge {
    let (ours, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut b = IoBridge::new_with_frames_for_test(ours, server, rx);
    b.scheduler.resize(4, 4);
    b.metrics_tracker.resize(4, 4);
    b
}

/// A frame whose every tile differs, so a tile decoded from the wrong place
/// cannot match by accident.
fn frame() -> Vec<u8> {
    (0..W * W)
        .flat_map(|i| {
            let (x, y) = (i % W, i / W);
            [
                (x * 3 + y) as u8,
                (y * 5 + x / 2) as u8,
                ((x ^ y) * 7) as u8,
                255,
            ]
        })
        .collect()
}

fn dispatch(
    b: &mut IoBridge,
    pixels: &[u8],
    dirty: &[(u32, u32)],
    palrle: Option<&mut std::collections::HashMap<(u32, u32), Vec<u8>>>,
) {
    let grid = crate::tile::TileGrid::new(W, W);
    let _ = b.dispatch_dirty_tiles_via_scheduler(
        dirty,
        &grid,
        TileDispatchFrame {
            pixels,
            stride: W * 4,
            seq: 1,
            timestamp_us: 0,
        },
        1200,
        SchedulerEmissionPolicy::GpuClassifierDriven,
        palrle,
    );
    // The dispatch's own budget lets only the first passes out; flush the
    // rest under the same frame sequence so every pass is in the cache.
    b.drain_scheduler_into_quinn(1, 0, 1200, usize::MAX);
}

/// Rebuild the tile from the Cdf53 passes the emitter sent for it (cached by
/// key, header stripped), using every pass. Exact equality with the source
/// tile is the lossless contract.
fn decode_from_scheduler(b: &IoBridge, tx: u8, ty: u8) -> Vec<u8> {
    use crate::transport::reliable_emitter::EmitKey;
    let mut sent: Vec<(u8, Vec<u8>)> = Vec::new();
    for p in 0..cdf53::CDF53_PASS_COUNT as u8 {
        let key = EmitKey::new(1 | TILE_DATAGRAM_FLAG, tx, ty, p);
        if let Some(e) = b.reliable_emitter.cache.get(&key) {
            assert_eq!(e.fragments.len(), 1, "a pass fits one datagram");
            sent.push((p, e.fragments[0][24..].to_vec()));
        }
    }
    assert!(!sent.is_empty(), "no Cdf53 pass was sent for ({tx},{ty})");
    // Sparse passes: pass 0 names which of the others are present.
    let present = u16::from_be_bytes([sent[0].1[0], sent[0].1[1]]);
    let mut dense: Vec<Vec<u8>> = Vec::new();
    let mut it = sent.into_iter();
    for p in 0..cdf53::CDF53_PASS_COUNT as u8 {
        if present & (1 << p) != 0 {
            dense.push(it.next().expect("a present pass was sent").1);
        } else {
            // An absent pass is an all-zero bit-plane: three empty channels.
            dense.push(vec![0, 1, 0xFF, 0, 1, 0xFF, 0, 1, 0xFF]);
        }
    }
    let refs: Vec<&[u8]> = dense.iter().map(|p| p.as_slice()).collect();
    cdf53::inverse(&cdf53::decode_passes(&refs))
}

fn bgr(tile_bgra: &[u8]) -> Vec<u8> {
    tile_bgra
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

fn assert_exact_cdf53(b: &IoBridge, pixels: &[u8], tx: u8, ty: u8) {
    let grid = crate::tile::TileGrid::new(W, W);
    let want = bgr(&grid.extract_tile(pixels, W * 4, tx as u32, ty as u32));
    assert_eq!(
        decode_from_scheduler(b, tx, ty),
        want,
        "tile ({tx},{ty}) must decode exactly"
    );
    assert!(
        b.scheduler
            .peek_for_test()
            .iter()
            .all(|w| w.codec != Codec::Raw),
        "no Raw work may be queued"
    );
}

/// A PalRle-classified tile whose payload was never made (palette table full,
/// frame palette overflowed) is the case the table-full comment named.
#[tokio::test]
async fn a_palrle_tile_without_a_payload_goes_out_as_exact_cdf53() {
    let mut b = bridge();
    let px = frame();
    b.metrics_tracker.get_mut(2, 1).codec_state = CodecState::PalRle { palette_id: 3 };
    let mut none = std::collections::HashMap::new();
    dispatch(&mut b, &px, &[(2, 1)], Some(&mut none));
    assert_exact_cdf53(&b, &px, 2, 1);
}

/// With a payload the tile stays PalRle: the fallback must not steal it.
#[tokio::test]
async fn a_palrle_tile_with_a_payload_stays_palrle() {
    let mut b = bridge();
    let px = frame();
    b.metrics_tracker.get_mut(2, 1).codec_state = CodecState::PalRle { palette_id: 3 };
    let mut have = std::collections::HashMap::new();
    have.insert((2u32, 1u32), vec![0u8, 3, 1, 2, 3, 4]);
    dispatch(&mut b, &px, &[(2, 1)], Some(&mut have));
    let q = b.scheduler.peek_for_test();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].codec, Codec::PalRle);
}

/// H.264-state tile in a tile-codec frame (mode hysteresis, or a client with
/// no H.264): no tile encoder exists for it.
#[tokio::test]
async fn an_h264_state_tile_goes_out_as_exact_cdf53() {
    let mut b = bridge();
    let px = frame();
    b.metrics_tracker.get_mut(1, 3).codec_state = CodecState::H264 { frames_in_h264: 2 };
    dispatch(&mut b, &px, &[(1, 3)], None);
    assert_exact_cdf53(&b, &px, 1, 3);
}

#[tokio::test]
async fn a_skip_state_dirty_tile_goes_out_as_exact_cdf53() {
    let mut b = bridge();
    let px = frame();
    b.metrics_tracker.get_mut(0, 0).codec_state = CodecState::Skip;
    dispatch(&mut b, &px, &[(0, 0)], None);
    assert_exact_cdf53(&b, &px, 0, 0);
}

/// A dirty tile still marked PixelPerfect has changed since it was refined:
/// it must be re-sent, and there is no tile encoder for the state.
#[tokio::test]
async fn a_pixel_perfect_state_dirty_tile_goes_out_as_exact_cdf53() {
    let mut b = bridge();
    let px = frame();
    b.metrics_tracker.get_mut(3, 3).codec_state = CodecState::PixelPerfect;
    dispatch(&mut b, &px, &[(3, 3)], None);
    assert_exact_cdf53(&b, &px, 3, 3);
}

/// Several fallback tiles at once, mixed with a Solid one that must stay Solid.
#[tokio::test]
async fn mixed_tiles_route_independently() {
    let mut b = bridge();
    let px = frame();
    b.metrics_tracker.get_mut(0, 0).codec_state = CodecState::Solid;
    b.metrics_tracker.get_mut(1, 0).codec_state = CodecState::Skip;
    b.metrics_tracker.get_mut(2, 0).codec_state = CodecState::H264 { frames_in_h264: 1 };
    dispatch(&mut b, &px, &[(0, 0), (1, 0), (2, 0)], None);
    assert_exact_cdf53(&b, &px, 1, 0);
    assert_exact_cdf53(&b, &px, 2, 0);
    let solid: Vec<_> = b
        .scheduler
        .peek_for_test()
        .into_iter()
        .filter(|w| w.codec == Codec::Solid)
        .collect();
    assert_eq!(solid.len(), 1);
}

/// What the classifier can hand a dirty tile. `PixelPerfect` is never an
/// output of `classify_tile`, so a dirty tile reaches the dispatch in that
/// state only if something else set it between classification and dispatch;
/// the routing above covers that case regardless.
#[test]
fn the_classifier_never_outputs_pixel_perfect() {
    use crate::tile::classifier::classify_tile;
    use crate::tile::TileMetrics;
    use proptest::prelude::*;
    let prevs = [
        CodecState::Skip,
        CodecState::H264 { frames_in_h264: 3 },
        CodecState::PalRle { palette_id: 1 },
        CodecState::Solid,
        CodecState::Cdf53 {
            passes_sent: 2,
            present_passes: 0x3FFF,
        },
        CodecState::PixelPerfect,
    ];
    proptest!(ProptestConfig::with_cases(2000), |(
        freq in 0.0f32..60.0,
        mag in 0.0f32..1.0,
        idle in 0u32..100,
        uc in prop_oneof![0u16..40, Just(crate::tile::UNIQUE_COLORS_UNKNOWN)],
        prev in 0usize..6,
    )| {
        let m = TileMetrics {
            change_freq_hz: freq,
            change_magnitude: mag,
            idle_frames: idle,
            unique_colors: uc,
            ..Default::default()
        };
        let out = classify_tile(&m, &prevs[prev]);
        prop_assert!(!matches!(out, CodecState::PixelPerfect));
    });
}
