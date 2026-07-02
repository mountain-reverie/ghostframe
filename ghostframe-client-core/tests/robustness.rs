//! Task 14: adversarial robustness proptests.
//!
//! `ClientCore::handle_datagram` must never panic on attacker-controlled
//! input: arbitrary bytes, truncated-but-otherwise-valid datagrams, and
//! single-bit-flipped valid datagrams. These tests use proptest's default
//! case counts (not raised).

use proptest::prelude::*;

use ghostframe_client_core::{ClientConfig, ClientCore};
use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};

fn test_core() -> ClientCore {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
        },
        0,
    );
    while core.poll_transmit(0).is_some() {}
    core
}

/// A valid CDF53 pass payload whose three channels each RLE-decode to 128
/// zero bytes: `[u16 BE len=1][0xFF]` per channel.
fn valid_cdf53_payload() -> Vec<u8> {
    let mut p = Vec::new();
    for _ in 0..3 {
        p.extend_from_slice(&[0x00, 0x01, 0xFF]);
    }
    p
}

/// Build a valid, multi-fragment CDF53 tile datagram set (small MTU so we
/// get more than one fragment to exercise fragmentation edge cases too).
fn valid_cdf53_datagram() -> Vec<u8> {
    let dgs = fragment_tile(
        &TileFragmentInputs {
            frame_seq: 1 | TILE_DATAGRAM_FLAG,
            tile_x: 2,
            tile_y: 3,
            codec: Codec::Cdf53,
            generation: 1,
            pass: 0,
            timestamp_us: 0,
        },
        &valid_cdf53_payload(),
        1200,
    );
    dgs.into_iter().next().unwrap()
}

/// Drive timers a couple of steps without panicking.
fn drive_timers(core: &mut ClientCore) {
    for _ in 0..2 {
        if let Some(deadline) = core.poll_timeout() {
            let _ = core.on_timeout(deadline);
        }
        while core.poll_transmit(0).is_some() {}
    }
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..2000)) {
        let mut core = test_core();
        let _ = core.handle_datagram(&data, 0);
        while core.poll_transmit(0).is_some() {}
        drive_timers(&mut core);
    }

    #[test]
    fn truncated_valid_datagrams_never_panic(cut in 0usize..100) {
        let dg = valid_cdf53_datagram();
        let cut = cut.min(dg.len());
        let mut core = test_core();
        let _ = core.handle_datagram(&dg[..cut], 0);
        while core.poll_transmit(0).is_some() {}
        drive_timers(&mut core);
    }

    #[test]
    fn bitflipped_datagrams_never_panic(pos in 0usize..200, bit in 0u8..8) {
        let mut dg = valid_cdf53_datagram();
        let pos = pos % dg.len();
        dg[pos] ^= 1 << bit;

        let mut core = test_core();
        let evs = core.handle_datagram(&dg, 0);
        for e in &evs {
            if let ghostframe_client_core::Event::TileReady { rgba, .. } = e {
                // Flipping a bit inside the TileHeader's packed
                // codec/lz4 byte (offset 18) can change the decoded codec
                // — e.g. Cdf53 (5) -> Raw (4) via a single bit flip in this
                // fixture, since `packed = (codec << 1) | lz4`. Codec::Raw's
                // RGBA output size is `payload.len() / 4 * 4` (whatever
                // fits in the surviving, possibly-truncated payload), NOT a
                // fixed 4096, so a strict `== 4096` assertion is too strong
                // for arbitrary single-bit flips and was a genuine test bug
                // (not a `src/` bug) caught by this proptest's minimal
                // failing case `pos = 18, bit = 1`.
                //
                // For Solid/PalRle/Cdf53 the decoder always emits exactly
                // 4096 bytes (32x32x4) by construction; Raw is the only
                // codec whose output size tracks payload length. The
                // invariant that must hold for ALL codecs, including
                // adversarial reinterpretation via bit flips, is: the
                // buffer is a well-formed whole-pixel RGBA buffer that
                // never exceeds one full 32x32 tile (4096 bytes) — no
                // partial pixels, no over-sized allocation.
                prop_assert_eq!(rgba.len() % 4, 0, "RGBA buffer must hold whole pixels");
                prop_assert!(rgba.len() <= 4096, "RGBA buffer must never exceed one full 32x32 tile");
            }
        }
        while core.poll_transmit(0).is_some() {}
        drive_timers(&mut core);
    }
}
