// SPDX-License-Identifier: MIT OR Apache-2.0
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
        if let Some(rgba) = st.integrate(0, 0, &pre) {
            last = rgba;
        }
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
        if let Some(rgba) = st.integrate(0, 0, &pre) {
            last = rgba;
        }
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
    for (i, pass) in passes.iter().enumerate().take(4) {
        let pre = prevalidate_cdf53(pass, 1, i as u8).unwrap();
        st.integrate(0, 0, &pre);
    }

    // Now passes arrive for a *new* generation (2). Prior planes must be
    // discarded entirely -- not blended with the new generation's data.
    // Its pass 0 alone carries no picture: nothing is drawn, so the old
    // content stays on screen rather than flashing black.
    let gen2: Vec<_> = (0..10)
        .map(|i| prevalidate_cdf53(&passes[i], 2, i as u8).unwrap())
        .collect();
    assert_eq!(st.integrate(0, 0, &gen2[0]), None);
    let mut result = None;
    for pre in &gen2[1..] {
        result = st.integrate(0, 0, pre).or(result);
    }

    // Expected: what a fresh state produces from the same gen-2 passes.
    let mut fresh = Cdf53TileState::new();
    let mut expected = None;
    for pre in &gen2 {
        expected = fresh.integrate(0, 0, pre).or(expected);
    }

    assert!(result.is_some(), "premise: ten passes draw something");
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

    // Pass 1 arrives before pass 0: there is no contiguous prefix, so there
    // is nothing to draw. (This used to return inverse() of all-zero
    // coefficients -- a black tile, painted over the old content.)
    let mut st = Cdf53TileState::new();
    let pre1 = prevalidate_cdf53(&passes[1], 5, 1).unwrap();
    assert_eq!(st.integrate(0, 0, &pre1), None);

    let mut fresh = Cdf53TileState::new();

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
    let mut drawn = None;
    for (i, pass) in passes.iter().enumerate().take(10) {
        let pre = prevalidate_cdf53(pass, 3, i as u8).unwrap();
        drawn = st.integrate(0, 0, &pre).or(drawn);
    }
    assert!(drawn.is_some(), "premise: ten passes draw something");

    // Re-deliver pass 9: the picture cannot change, so nothing is redrawn.
    let dup = prevalidate_cdf53(&passes[9], 3, 9).unwrap();
    assert_eq!(
        st.integrate(0, 0, &dup),
        None,
        "a duplicate must not redraw"
    );

    // And it left the state alone: the next pass draws what a clean
    // in-order run draws.
    let pre10 = prevalidate_cdf53(&passes[10], 3, 10).unwrap();
    let mut fresh = Cdf53TileState::new();
    let mut expected = None;
    for (i, pass) in passes.iter().enumerate().take(11) {
        let pre = prevalidate_cdf53(pass, 3, i as u8).unwrap();
        expected = fresh.integrate(0, 0, &pre).or(expected);
    }
    assert!(expected.is_some(), "premise: pass 10 adds detail");
    assert_eq!(st.integrate(0, 0, &pre10), expected);
}

#[test]
fn reset_clears_all_tile_state() {
    let tile = make_tile();
    let coeffs = cdf53::forward(&tile);
    let passes = cdf53::encode_passes(&coeffs);

    let mut st = Cdf53TileState::new();
    for (i, pass) in passes.iter().enumerate().take(10) {
        st.integrate(0, 0, &prevalidate_cdf53(pass, 1, i as u8).unwrap());
    }
    st.reset();

    // After reset, re-integrating at a *different* generation must behave
    // exactly as on a fresh state (no residual plane leaking through).
    let gen2: Vec<_> = (0..10)
        .map(|i| prevalidate_cdf53(&passes[i], 2, i as u8).unwrap())
        .collect();
    let (mut after_reset, mut expected) = (None, None);
    let mut fresh = Cdf53TileState::new();
    for pre in &gen2 {
        after_reset = st.integrate(0, 0, pre).or(after_reset);
        expected = fresh.integrate(0, 0, pre).or(expected);
    }

    assert!(expected.is_some(), "premise: ten passes draw something");
    assert_eq!(after_reset, expected);
}

/// A tile seen while its passes are still arriving -- a typical desktop tile,
/// a flat light one, and the noisy fixture -- dense and sparse. The user-visible
/// bug: every new version of a tile flashed black before its content appeared,
/// because pass 0 (signs only) and any all-zero prefix decode to coefficients
/// of 0, which `inverse` (no level shift) turns into black pixels.
#[test]
fn a_tile_is_never_drawn_black_while_its_passes_arrive() {
    let light: Vec<u8> = (0..1024).flat_map(|_| [0xE0, 0xE0, 0xE0, 0xFF]).collect();
    let text_on_white: Vec<u8> = (0..1024)
        .flat_map(|px| {
            let ink = (px % 32) % 7 == 0 && (px / 32) % 5 < 3;
            let v = if ink { 0x20 } else { 0xF8 };
            [v, v, v, 0xFF]
        })
        .collect();
    for (name, tile) in [
        ("noise", make_tile()),
        ("light", light),
        ("text", text_on_white),
    ] {
        let coeffs = cdf53::forward(&tile);
        let dense: Vec<(u8, Vec<u8>)> = cdf53::encode_passes(&coeffs)
            .into_iter()
            .enumerate()
            .map(|(i, p)| (i as u8, p))
            .collect();
        let (_present, sparse) = cdf53::encode_passes_sparse(&coeffs);
        for (encoding, passes) in [("dense", dense), ("sparse", sparse)] {
            let mut st = Cdf53TileState::new();
            let mut drawn = 0;
            let mut last = None;
            for (idx, payload) in &passes {
                let pre = prevalidate_cdf53(payload, 1, *idx).unwrap();
                if let Some(rgba) = st.integrate(0, 0, &pre) {
                    drawn += 1;
                    let brightest = rgba.chunks_exact(4).map(|p| p[0].max(p[1]).max(p[2])).max();
                    assert!(
                        brightest > Some(16),
                        "{name}/{encoding}: drew an all-black tile after pass {idx}"
                    );
                    last = Some(rgba);
                }
            }
            assert!(drawn > 0, "{name}/{encoding}: never drew anything");
            if name == "noise" {
                assert!(
                    drawn > 1,
                    "{name}/{encoding}: drawn only once -- progressive refinement lost"
                );
            }
            let exact = cdf53::inverse(&coeffs);
            assert_rgba_matches_bgr(&last.unwrap(), &exact);
        }
    }
}

/// The other side of the rule above: a tile that really is black is still
/// drawn, once every pass is accounted for.
#[test]
fn a_black_tile_is_drawn_once_complete() {
    let black: Vec<u8> = (0..1024).flat_map(|_| [0, 0, 0, 0xFF]).collect();
    let coeffs = cdf53::forward(&black);

    // Sparse: pass 0 alone says no other pass exists, so it is complete.
    let (_present, sparse) = cdf53::encode_passes_sparse(&coeffs);
    assert_eq!(sparse.len(), 1, "premise: a black tile is pass 0 only");
    let mut st = Cdf53TileState::new();
    let drawn = st
        .integrate(0, 0, &prevalidate_cdf53(&sparse[0].1, 1, 0).unwrap())
        .expect("complete black tile must be drawn");
    assert!(drawn.chunks_exact(4).all(|p| p[..3] == [0, 0, 0]));

    // Dense: drawn when the last pass lands, not before.
    let dense = cdf53::encode_passes(&coeffs);
    let mut st = Cdf53TileState::new();
    for (i, p) in dense.iter().enumerate() {
        let got = st.integrate(0, 0, &prevalidate_cdf53(p, 1, i as u8).unwrap());
        assert_eq!(got.is_some(), i == dense.len() - 1, "pass {i}");
    }
}

/// A pass held behind a gap changes nothing on screen, so it draws nothing.
#[test]
fn a_pass_behind_a_gap_draws_nothing() {
    let coeffs = cdf53::forward(&make_tile());
    let passes = cdf53::encode_passes(&coeffs);
    let mut st = Cdf53TileState::new();
    let mut drawn = None;
    for (i, pass) in passes.iter().enumerate().take(10) {
        drawn = st
            .integrate(0, 0, &prevalidate_cdf53(pass, 1, i as u8).unwrap())
            .or(drawn);
    }
    assert!(drawn.is_some(), "premise: the tile is on screen");
    // Pass 10 missing; pass 12 cannot be used yet.
    assert_eq!(
        st.integrate(0, 0, &prevalidate_cdf53(&passes[12], 1, 12).unwrap()),
        None
    );
    // Once the gap fills, both are used at once.
    assert!(st
        .integrate(0, 0, &prevalidate_cdf53(&passes[10], 1, 10).unwrap())
        .is_some());
}
