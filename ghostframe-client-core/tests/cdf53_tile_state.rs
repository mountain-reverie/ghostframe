//! Oracle tests for CPU CDF53 per-tile accumulation → RGBA reconstruction.
//! Uses `ghostframe_protocol::codec::cdf53::{forward, encode_passes, inverse}`
//! as the server-side oracle to build accepted passes and expected pixels.

use ghostframe_client_core::cdf53_prevalidate::prevalidate_cdf53;
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_protocol::codec::cdf53;

/// Deterministic 32x32 BGRA tile, values covering the full byte range.
fn make_tile() -> Vec<u8> {
    (0..4096).map(|i| (i * 7 % 251) as u8).collect()
}

fn assert_rgba_matches_bgr(rgba: &[u8], expected_bgr: &[u8]) {
    assert_eq!(rgba.len(), 4096);
    for px in 0..1024 {
        assert_eq!(
            &rgba[px * 4..px * 4 + 3],
            &[
                expected_bgr[px * 3 + 2],
                expected_bgr[px * 3 + 1],
                expected_bgr[px * 3],
            ],
            "pixel {px} RGB mismatch"
        );
        assert_eq!(rgba[px * 4 + 3], 255, "pixel {px} alpha must be 255");
    }
}

#[test]
fn fourteen_passes_reconstruct_exactly() {
    let tile = make_tile();
    let coeffs = cdf53::forward(&tile);
    let passes = cdf53::encode_passes(&coeffs);

    let mut st = Cdf53TileState::new();
    let mut last = Vec::new();
    for (i, p) in passes.iter().enumerate() {
        let pre = prevalidate_cdf53(p, 1, i as u8).unwrap();
        last = st.integrate(0, 0, &pre);
    }

    let expected_bgr = cdf53::inverse(&coeffs);
    assert_rgba_matches_bgr(&last, &expected_bgr);
}

#[test]
fn mid_k_seven_passes_match_decode_passes_oracle() {
    let tile = make_tile();
    let coeffs = cdf53::forward(&tile);
    let passes = cdf53::encode_passes(&coeffs);

    // Integrate exactly K=7 passes (0..7) through Cdf53TileState.
    let mut st = Cdf53TileState::new();
    let mut last = Vec::new();
    for (i, p) in passes.iter().take(7).enumerate() {
        let pre = prevalidate_cdf53(p, 1, i as u8).unwrap();
        last = st.integrate(0, 0, &pre);
    }

    // Payload-based oracle: decode_passes over the first 7 raw encoded
    // passes, then inverse() + BGR->RGBA, matching integrate()'s mapping.
    let pass_refs: Vec<&[u8]> = passes.iter().take(7).map(|p| p.as_slice()).collect();
    let oracle_coeffs = cdf53::decode_passes(&pass_refs);
    let oracle_bgr = cdf53::inverse(&oracle_coeffs);

    assert_rgba_matches_bgr(&last, &oracle_bgr);
}

#[test]
fn generation_bump_discards_stale_planes() {
    let tile = make_tile();
    let coeffs = cdf53::forward(&tile);
    let passes = cdf53::encode_passes(&coeffs);

    let mut st = Cdf53TileState::new();
    // Integrate passes 0..3 at generation 1.
    for i in 0..4usize {
        let pre = prevalidate_cdf53(&passes[i], 1, i as u8).unwrap();
        st.integrate(0, 0, &pre);
    }

    // Now a pass 0 arrives for a *new* generation (2). Prior planes must be
    // discarded entirely — not blended with the new generation's data.
    let pre_gen2 = prevalidate_cdf53(&passes[0], 2, 0).unwrap();
    let result = st.integrate(0, 0, &pre_gen2);

    // Expected: what a fresh state would produce from just pass 0 of gen 2.
    let mut fresh = Cdf53TileState::new();
    let expected = fresh.integrate(0, 0, &pre_gen2);

    assert_eq!(
        result, expected,
        "generation bump must discard prior planes, not blend them"
    );
}

#[test]
fn out_of_order_arrival_uses_only_contiguous_prefix() {
    let tile = make_tile();
    let coeffs = cdf53::forward(&tile);
    let passes = cdf53::encode_passes(&coeffs);

    // Pass 1 arrives before pass 0: reconstruction must ignore pass 1 (gap
    // at pass 0) and behave as if no pass has been received yet.
    let mut st = Cdf53TileState::new();
    let pre1 = prevalidate_cdf53(&passes[1], 5, 1).unwrap();
    let result_before_pass0 = st.integrate(0, 0, &pre1);

    let mut fresh = Cdf53TileState::new();
    // Zero passes held → `integrate` has no contiguous prefix to draw
    // coefficients from → all coefficients 0 → inverse() of all-zero
    // coefficients.
    let zero_coeffs = vec![0i16; cdf53::CDF53_TOTAL_COEFFS];
    let expected_zero_bgr = cdf53::inverse(&zero_coeffs);
    assert_rgba_matches_bgr(&result_before_pass0, &expected_zero_bgr);

    // Now pass 0 arrives: contiguous prefix is now [0], pass 1 fills the gap.
    let pre0 = prevalidate_cdf53(&passes[0], 5, 0).unwrap();
    let result_after_pass0 = st.integrate(0, 0, &pre0);

    // Expected: what a fresh state gets from integrating pass 0 then pass 1
    // in order (both now contiguous and available).
    fresh.reset();
    let e0 = prevalidate_cdf53(&passes[0], 5, 0).unwrap();
    fresh.integrate(0, 0, &e0);
    let e1 = prevalidate_cdf53(&passes[1], 5, 1).unwrap();
    let expected_after = fresh.integrate(0, 0, &e1);

    assert_eq!(result_after_pass0, expected_after);
}

#[test]
fn duplicate_pass_arrival_is_idempotent() {
    let tile = make_tile();
    let coeffs = cdf53::forward(&tile);
    let passes = cdf53::encode_passes(&coeffs);

    let mut st = Cdf53TileState::new();
    let pre0 = prevalidate_cdf53(&passes[0], 3, 0).unwrap();
    let first = st.integrate(0, 0, &pre0);

    // Re-deliver the same pass 0 payload again.
    let pre0_dup = prevalidate_cdf53(&passes[0], 3, 0).unwrap();
    let second = st.integrate(0, 0, &pre0_dup);

    assert_eq!(first, second, "duplicate pass delivery must be idempotent");
}

#[test]
fn reset_clears_all_tile_state() {
    let tile = make_tile();
    let coeffs = cdf53::forward(&tile);
    let passes = cdf53::encode_passes(&coeffs);

    let mut st = Cdf53TileState::new();
    let pre0 = prevalidate_cdf53(&passes[0], 1, 0).unwrap();
    st.integrate(0, 0, &pre0);
    st.reset();

    // After reset, re-integrating pass 0 at a *different* generation must
    // behave exactly as on a fresh state (no residual plane leaking through).
    let pre0_gen2 = prevalidate_cdf53(&passes[0], 2, 0).unwrap();
    let after_reset = st.integrate(0, 0, &pre0_gen2);

    let mut fresh = Cdf53TileState::new();
    let expected = fresh.integrate(0, 0, &pre0_gen2);

    assert_eq!(after_reset, expected);
}
