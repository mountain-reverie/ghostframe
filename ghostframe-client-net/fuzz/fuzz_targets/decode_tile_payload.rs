#![no_main]

//! Fuzzes the tile decoders directly, with attacker-controlled payload bytes.
//!
//! `handle_udp` is the other target here, but it fuzzes an unconnected
//! `ClientNet`: with no established connection, quinn-proto discards packets
//! during header parsing, so tile payloads are never reached (measured
//! `cov: 674` after 20k runs, all of it in packet-header territory). quinn is
//! also already fuzzed heavily upstream.
//!
//! These decoders are the interesting surface for *this* protocol: they take
//! bytes straight off the wire and do index arithmetic, run-length expansion
//! and palette lookups against them. They are also pure functions, so unlike
//! a browserless scene a crash found here reproduces exactly from its input.

use ghostframe_client_core::cdf53_prevalidate::prevalidate_cdf53;
use ghostframe_client_core::fragment_parity::decode_parity_payload;
use ghostframe_client_core::pal_rle_decode::{decode_pal_rle_tile, prevalidate_pal_rle};
use ghostframe_client_core::palette_shadow::PaletteShadow;
use ghostframe_protocol::codec::solid::decode_solid;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // First two bytes steer the CDF53 header fields; the rest is payload. A
    // short input still exercises the "too short" paths, which is why this
    // does not bail out on len < 2.
    let (generation, pass_idx) = match data {
        [a, b, ..] => (*a & 0x0F, *b & 0x0F),
        _ => (0, 0),
    };
    let payload = if data.len() > 2 { &data[2..] } else { data };

    // None of these may panic on any input.
    let _ = decode_solid(payload);
    let _ = prevalidate_cdf53(payload, generation, pass_idx);
    let _ = decode_parity_payload(payload);

    let shadow = PaletteShadow::new();
    let _ = prevalidate_pal_rle(payload, &shadow);

    // The full decode mutates palette state, so it gets its own shadow and
    // table; a 16 KiB table on the stack would risk overflow, hence the box.
    let mut shadow = PaletteShadow::new();
    let mut palettes = Box::new([[[0u8; 4]; 16]; 256]);
    let _ = decode_pal_rle_tile(payload, &mut shadow, &mut palettes);
});
