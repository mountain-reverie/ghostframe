//! An oracle for the **GPU** Cdf53 reconstruction under sparse pass sets.
//!
//! # Why this exists
//!
//! There are two Cdf53 decoders in this project and only one of them was
//! taught about sparse encoding:
//!
//! - `Cdf53TileState::integrate` (Rust, `cdf53_tile_state.rs`) reads pass 0's
//!   `present_passes` bitmap and **backfills every absent pass with an
//!   explicit all-zero plane**, which is the content an empty pass would have
//!   carried. Its contiguous prefix therefore reaches 14 and no midpoint
//!   correction is applied.
//! - The browser's GPU path (`webgpu/cdf53.ts` + `cdf53_inverse_*.wgsl`) is
//!   what actually renders in production. It never sees the bitmap -- the
//!   wasm boundary hands JS only `{ pass_idx, bit_planes }` -- and derives
//!   its K as `passesProcessed = max(passIdx + 1)`.
//!
//! Both then apply the *same* midpoint formula. So they agree exactly while
//! passes are still arriving, and they agree once the highest present pass is
//! 13. They diverge when the encoder skips **trailing** passes: the Rust side
//! backfills them and concludes "lossless", while the GPU concludes "K passes
//! of 14" and adds a midpoint for low bits that are known to be zero.
//!
//! This file models the GPU's arithmetic in Rust so the divergence is
//! measurable without a GPU, and pins the Rust decoder as the reference.
//!
//! # Fidelity of the model
//!
//! `gpu_model_coefficients` mirrors three pieces of shipped client code and
//! must be kept in step with them:
//!
//! - bit placement: `cdf53_integrate.wgsl`, `bit_pos = 13u - pass_idx`, and
//!   `pass_idx == 0` routed to the sign buffer.
//! - K: `Cdf53Pipeline.uploadBatch`, `candidate = e.passIdx + 1` kept as a
//!   running max, reset to 0 on a generation change.
//! - midpoint: `cdf53_inverse_l3.wgsl`'s `read_coeff` --
//!   `if (raw_mag != 0 && passes >= 2u && passes < 14u) { mag = raw_mag +
//!   (1 << (14 - passes - 1)) }`.

use ghostframe_client_core::cdf53_prevalidate::prevalidate_cdf53;
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_protocol::codec::cdf53;

const COEFFS_PER_CHANNEL: usize = 1024;
const CHANNELS: usize = 3;
const PLANE_BYTES_PER_CHANNEL: usize = 128;
const PASS_COUNT: usize = 14;
const FULL_PASS_MASK: u16 = (1 << 14) - 1;

/// Model of the GPU pipeline's coefficient reconstruction. See the module
/// docs for the three source sites this mirrors.
///
/// `present` is the tile's bitmap once pass 0 has delivered it, or 0 while
/// it is still unknown -- the same input `computePassesProcessed` takes.
fn gpu_model_coefficients(arrived: &[(u8, Vec<u8>)], present: u16) -> Vec<i16> {
    let total = CHANNELS * COEFFS_PER_CHANNEL;
    let mut magnitudes = vec![0u32; total];
    let mut signs = vec![false; total];

    // `passesProcessed`, mirroring `computePassesProcessed` in
    // `webgpu/cdf53.ts`.
    let mut passes_processed: u32 = 0;
    let mut received_mask: u16 = 0;

    for (pass_idx, bit_planes) in arrived {
        received_mask |= 1u16 << *pass_idx;
        passes_processed = if present & FULL_PASS_MASK != 0 {
            // A skipped pass is resolved -- known zero, not undelivered --
            // so K is the contiguous prefix of resolved passes.
            let resolved = (received_mask | !present) & FULL_PASS_MASK;
            (0..PASS_COUNT as u32)
                .take_while(|k| resolved & (1u16 << k) != 0)
                .count() as u32
        } else {
            passes_processed.max(u32::from(*pass_idx) + 1)
        };
        for ch in 0..CHANNELS {
            let plane_offset = ch * PLANE_BYTES_PER_CHANNEL;
            let coeff_offset = ch * COEFFS_PER_CHANNEL;
            for i in 0..COEFFS_PER_CHANNEL {
                let byte = bit_planes[plane_offset + i / 8];
                if (byte >> (i % 8)) & 1 == 0 {
                    continue;
                }
                if *pass_idx == 0 {
                    signs[coeff_offset + i] = true;
                } else {
                    let bit_pos = 13 - u32::from(*pass_idx);
                    magnitudes[coeff_offset + i] |= 1u32 << bit_pos;
                }
            }
        }
    }

    // read_coeff's midpoint correction.
    let midpoint: u32 = if (2..14).contains(&passes_processed) {
        let unknown_bits = 14 - passes_processed;
        1u32 << (unknown_bits - 1)
    } else {
        0
    };

    (0..total)
        .map(|i| {
            let mut mag = magnitudes[i];
            if mag != 0 && midpoint != 0 {
                mag = mag.saturating_add(midpoint);
            }
            let mag = mag.min(i16::MAX as u32) as i16;
            if signs[i] {
                -mag
            } else {
                mag
            }
        })
        .collect()
}

/// Prevalidate every sparse pass into the `(pass_idx, bit_planes)` pairs the
/// browser hands to `renderer.pushCdf53` -- the exact GPU input.
fn arrivals(payloads: &[(u8, Vec<u8>)]) -> Vec<(u8, Vec<u8>)> {
    payloads
        .iter()
        .map(|(idx, payload)| {
            let pre = prevalidate_cdf53(payload, 1, *idx).expect("pass prevalidates");
            (*idx, pre.bit_planes)
        })
        .collect()
}

/// What the Rust client renders: `integrate` every pass, take the last
/// result. Pass 0 first, so its bitmap is known before the rest land --
/// the ordering production uses.
fn rust_client_rgba(payloads: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut st = Cdf53TileState::new();
    let mut last = Vec::new();
    for (idx, payload) in payloads {
        let pre = prevalidate_cdf53(payload, 1, *idx).expect("pass prevalidates");
        last = st.integrate(0, 0, &pre);
    }
    last
}

fn bgr_to_rgba(bgr: &[u8]) -> Vec<u8> {
    let mut rgba = vec![255u8; COEFFS_PER_CHANNEL * 4];
    for px in 0..COEFFS_PER_CHANNEL {
        rgba[px * 4] = bgr[px * 3 + 2];
        rgba[px * 4 + 1] = bgr[px * 3 + 1];
        rgba[px * 4 + 2] = bgr[px * 3];
    }
    rgba
}

/// Largest absolute per-channel difference, ignoring alpha.
fn max_rgb_delta(a: &[u8], b: &[u8]) -> i32 {
    let mut worst = 0i32;
    for px in 0..COEFFS_PER_CHANNEL {
        for c in 0..3 {
            let d = (a[px * 4 + c] as i32 - b[px * 4 + c] as i32).abs();
            if d > worst {
                worst = d;
            }
        }
    }
    worst
}

// ---------------------------------------------------------------------------
// Content generators, chosen so the sparse encoder produces different
// present-sets: smooth content leaves trailing bit-planes empty, noisy
// content fills them.
// ---------------------------------------------------------------------------

/// Near-uniform: tiny variation around a mid grey. Coefficients stay small,
/// so both the high AND low magnitude planes are empty.
fn nearly_flat_tile() -> Vec<u8> {
    let mut t = vec![0u8; 32 * 32 * 4];
    for (i, px) in t.chunks_exact_mut(4).enumerate() {
        let v = 128u8.wrapping_add(((i / 97) % 2) as u8);
        px[0] = v;
        px[1] = v;
        px[2] = v;
        px[3] = 255;
    }
    t
}

/// Exactly uniform.
fn flat_tile() -> Vec<u8> {
    let mut t = vec![0u8; 32 * 32 * 4];
    for px in t.chunks_exact_mut(4) {
        px[0] = 64;
        px[1] = 96;
        px[2] = 160;
        px[3] = 255;
    }
    t
}

/// High-entropy: fills the low bit-planes, so nothing trails empty.
fn noisy_tile() -> Vec<u8> {
    let mut t = vec![0u8; 32 * 32 * 4];
    let mut rng: u32 = 0x1234_5678;
    for px in t.chunks_exact_mut(4) {
        rng = rng.wrapping_mul(48271).wrapping_add(1);
        px[0] = (rng >> 8) as u8;
        px[1] = (rng >> 16) as u8;
        px[2] = (rng >> 24) as u8;
        px[3] = 255;
    }
    t
}

fn present_set(present: u16) -> Vec<u8> {
    (0..14u8).filter(|i| present & (1 << i) != 0).collect()
}

/// Report the two reconstructions for one tile.
fn compare(name: &str, bgra: &[u8]) -> (i32, u16, u8) {
    let coeffs = cdf53::forward(bgra);
    let (present, sparse) = cdf53::encode_passes_sparse(&coeffs);
    let highest = present_set(present)
        .into_iter()
        .max()
        .expect("pass 0 present");

    let truth = bgr_to_rgba(&cdf53::inverse(&coeffs));
    let rust = rust_client_rgba(&sparse);
    let gpu = bgr_to_rgba(&cdf53::inverse(&gpu_model_coefficients(
        &arrivals(&sparse),
        present,
    )));

    let rust_err = max_rgb_delta(&truth, &rust);
    let gpu_err = max_rgb_delta(&truth, &gpu);
    println!(
        "{name}: present={:?} highest={highest} rust_err={rust_err} gpu_err={gpu_err}",
        present_set(present)
    );
    (gpu_err, present, highest)
}

/// The Rust decoder is the reference: with every present pass delivered it
/// reconstructs the tile exactly, for every content class.
#[test]
fn the_rust_decoder_is_exact_once_all_present_passes_arrive() {
    for (name, tile) in [
        ("flat", flat_tile()),
        ("nearly_flat", nearly_flat_tile()),
        ("noisy", noisy_tile()),
    ] {
        let coeffs = cdf53::forward(&tile);
        let (_present, sparse) = cdf53::encode_passes_sparse(&coeffs);
        let truth = bgr_to_rgba(&cdf53::inverse(&coeffs));
        let rust = rust_client_rgba(&sparse);
        assert_eq!(
            max_rgb_delta(&truth, &rust),
            0,
            "{name}: the Rust client must be lossless on a complete sparse pass set"
        );
    }
}

/// Content whose highest present pass is 13 leaves the GPU's K at 14, so its
/// midpoint correction is skipped and the two decoders agree. This is the
/// case production mostly hits (1300 of 1301 tile-generations measured), and
/// it is why the defect below stayed invisible.
#[test]
fn the_gpu_model_agrees_when_no_trailing_pass_is_skipped() {
    let (gpu_err, _present, highest) = compare("noisy", &noisy_tile());
    assert_eq!(
        highest, 13,
        "premise: noisy content must fill the last plane"
    );
    assert_eq!(
        gpu_err, 0,
        "with the last plane present the GPU applies no midpoint and must be exact"
    );
}

/// Regression guard for the defect this file was written to find.
///
/// When the encoder skips **trailing** bit-planes, the old rule
/// (`passesProcessed = max(passIdx + 1)`) under-counted: it read "K of 14
/// passes decoded, low bits unknown" where the truth is "those planes were
/// skipped *because* they are zero". `read_coeff` then added a midpoint the
/// Rust decoder correctly omits -- measured at 16/255 per channel on flat
/// content (present = {0,6,7,8} => K=9 => midpoint 2^4).
///
/// With the bitmap plumbed through to the GPU, a skipped pass is resolved
/// and K reaches 14, so no correction is applied to a tile that is in fact
/// losslessly reconstructed.
#[test]
fn the_gpu_is_exact_when_trailing_passes_are_skipped() {
    let (gpu_err, present, highest) = compare("flat", &flat_tile());
    assert!(
        highest < 13,
        "premise: this content must leave trailing planes empty; present={:?}",
        present_set(present)
    );
    assert_eq!(
        gpu_err,
        0,
        "GPU reconstruction is off by {gpu_err} per channel against a decoder \
         that is exact on the same input. A skipped trailing plane must count \
         as resolved (known zero), not undecoded, or read_coeff adds a \
         midpoint that should not be there. present={:?} highest={highest}",
        present_set(present)
    );
}

/// The *leading*-gap case, which is far more common than the trailing one
/// and is what the K rule change most affects in practice.
///
/// Sparse encoding skips passes 1..7 on essentially all real content
/// (measured: every `ContentClass` fixture, and 1300 of 1301 production
/// tile-generations, has present = {0, 5..13} or {0, 6..13}). So during the
/// whole refinement window -- which production spends seconds in, given
/// queued->ACK latencies of 1.6-2.3 s -- a tile has pass 0 plus some suffix.
///
/// Old rule: K = max(passIdx + 1). With only pass 0 in hand that is K=1, so
/// `read_coeff` applies no midpoint at all and the tile reconstructs from
/// bare accumulated bits.
///
/// New rule: passes 1..7 are resolved (known zero), so K=8 and the midpoint
/// for the genuinely-unknown low bits *is* applied -- which is what the Rust
/// decoder does in the same state, and what the SPIHT correction is for.
///
/// This measures both against the truth at each refinement step. The new
/// rule must be at least as good at every step.
///
/// **Measured result: they are identical at every step.** For the shape real
/// content produces -- absent passes all sitting *below* the lowest present
/// pass -- `max(passIdx + 1)` and the contiguous-prefix rule agree exactly,
/// because the received passes then form a contiguous run after the skipped
/// prefix. The two rules diverge only when a gap falls *between* received
/// passes, i.e. a trailing skip or a genuinely missing middle pass.
///
/// So the K fix is correct but has no practical effect on realistic content:
/// no `ContentClass` fixture, and only 1 of 1301 production tile-generations,
/// produces a shape where it matters. Recorded here so nobody credits it with
/// a visual improvement it cannot have caused.
#[test]
fn partial_refinement_is_no_worse_under_the_new_k_rule() {
    let bgra = noisy_tile();
    let coeffs = cdf53::forward(&bgra);
    let (present, sparse) = cdf53::encode_passes_sparse(&coeffs);
    let truth = bgr_to_rgba(&cdf53::inverse(&coeffs));
    let arrived_all = arrivals(&sparse);

    println!("partial-refinement, present={:?}", present_set(present));
    let mut regressions = Vec::new();
    for n in 1..=arrived_all.len() {
        let prefix = &arrived_all[..n];
        // New rule: bitmap known from pass 0 onward.
        let new_err = max_rgb_delta(
            &truth,
            &bgr_to_rgba(&cdf53::inverse(&gpu_model_coefficients(prefix, present))),
        );
        // Old rule: modelled by withholding the bitmap, which is exactly the
        // `present == 0` fallback branch -- max(passIdx + 1).
        let old_err = max_rgb_delta(
            &truth,
            &bgr_to_rgba(&cdf53::inverse(&gpu_model_coefficients(prefix, 0))),
        );
        println!(
            "  after {n} pass(es) (up to idx {}): old_err={old_err} new_err={new_err}",
            prefix.last().expect("non-empty").0
        );
        if new_err > old_err {
            regressions.push((n, old_err, new_err));
        }
    }
    assert!(
        regressions.is_empty(),
        "the new K rule made intermediate reconstruction worse at these \
         refinement steps (step, old_err, new_err): {regressions:?}"
    );
}
