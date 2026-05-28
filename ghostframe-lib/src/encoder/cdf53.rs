//! CPU reference implementation of CDF 5/3 wavelet codec.
//!
//! This is the algorithmic source of truth for the codec:
//!   - Unit-tests verify forward+inverse roundtrip (lossless).
//!   - M3.3a's Vulkan compute Stage 4 shaders verify GPU=CPU equivalence
//!     against this reference.
//!   - M3.3b's WGSL inverse compute shader is ported from this reference
//!     (NOT compiled to WASM — WebGPU compute is the deployed path).

/// Number of progressive passes emitted per Cdf53 tile.
/// = 1 sign-bit-plane + 13 magnitude bit-planes covering worst-case
/// 14-bit signed coefficients after 3 levels of CDF 5/3 lifting on
/// 8-bit input.
pub const CDF53_PASS_COUNT: usize = 14;

/// Per-tile coefficient count per channel: 1024 (32×32 tile).
pub const CDF53_COEFFS_PER_CHANNEL: usize = 1024;

/// Number of color channels encoded: BGR (alpha dropped, always 0xFF).
pub const CDF53_CHANNELS: usize = 3;

/// Total coefficients per tile = 3072.
pub const CDF53_TOTAL_COEFFS: usize = CDF53_CHANNELS * CDF53_COEFFS_PER_CHANNEL;

/// Per-channel bit-plane byte size = 1024 bits / 8 = 128 bytes.
const BIT_PLANE_BYTES_PER_CHANNEL: usize = CDF53_COEFFS_PER_CHANNEL / 8;

/// Forward CDF 5/3 lifting transform on a 32×32 BGRA tile.
/// Strips alpha and returns 3072 signed integer coefficients
/// in [B-coeffs (1024)][G-coeffs (1024)][R-coeffs (1024)] order.
/// Each channel's coefficients are laid out as:
///   [LL3 (16)][HL3 (16)][LH3 (16)][HH3 (16)]
///   [HL2 (64)][LH2 (64)][HH2 (64)]
///   [HL1 (256)][LH1 (256)][HH1 (256)]
pub fn forward(tile_bgra: &[u8]) -> Vec<i16> {
    assert_eq!(tile_bgra.len(), 32 * 32 * 4, "tile must be 32×32 BGRA");
    let mut output = vec![0i16; CDF53_TOTAL_COEFFS];

    for ch in 0..CDF53_CHANNELS {
        // Load channel into a 32×32 working buffer (i32 to absorb growth).
        let mut work = [[0i32; 32]; 32];
        for y in 0..32 {
            for x in 0..32 {
                let idx = (y * 32 + x) * 4 + ch;
                work[y][x] = tile_bgra[idx] as i32;
            }
        }

        // 3 levels of 2D decomposition.
        let mut size = 32;
        for _level in 0..3 {
            // Horizontal pass on the size×size LL subband.
            for y in 0..size {
                row_forward(&mut work[y], size);
            }
            // Vertical pass.
            for x in 0..size {
                let mut column = [0i32; 32];
                for y in 0..size {
                    column[y] = work[y][x];
                }
                col_forward(&mut column, size);
                for y in 0..size {
                    work[y][x] = column[y];
                }
            }
            size /= 2;
        }

        // Pack work[][] into channel layout. After 3 levels:
        //   work[0..4][0..4]       = LL3
        //   work[0..4][4..8]       = HL3
        //   work[4..8][0..4]       = LH3
        //   work[4..8][4..8]       = HH3
        //   work[0..8][8..16]      = HL2
        //   work[8..16][0..8]      = LH2
        //   work[8..16][8..16]     = HH2
        //   work[0..16][16..32]    = HL1
        //   work[16..32][0..16]    = LH1
        //   work[16..32][16..32]   = HH1
        let channel_offset = ch * CDF53_COEFFS_PER_CHANNEL;
        let mut out_idx = channel_offset;

        // LL3, HL3, LH3, HH3 (4×4 each → 16 each)
        for y in 0..4 { for x in 0..4 { output[out_idx] = work[y][x] as i16; out_idx += 1; } }       // LL3
        for y in 0..4 { for x in 4..8 { output[out_idx] = work[y][x] as i16; out_idx += 1; } }       // HL3
        for y in 4..8 { for x in 0..4 { output[out_idx] = work[y][x] as i16; out_idx += 1; } }       // LH3
        for y in 4..8 { for x in 4..8 { output[out_idx] = work[y][x] as i16; out_idx += 1; } }       // HH3
        // HL2, LH2, HH2 (8×8 each → 64 each)
        for y in 0..8  { for x in 8..16  { output[out_idx] = work[y][x] as i16; out_idx += 1; } }    // HL2
        for y in 8..16 { for x in 0..8   { output[out_idx] = work[y][x] as i16; out_idx += 1; } }    // LH2
        for y in 8..16 { for x in 8..16  { output[out_idx] = work[y][x] as i16; out_idx += 1; } }    // HH2
        // HL1, LH1, HH1 (16×16 each → 256 each)
        for y in 0..16  { for x in 16..32 { output[out_idx] = work[y][x] as i16; out_idx += 1; } }   // HL1
        for y in 16..32 { for x in 0..16  { output[out_idx] = work[y][x] as i16; out_idx += 1; } }   // LH1
        for y in 16..32 { for x in 16..32 { output[out_idx] = work[y][x] as i16; out_idx += 1; } }   // HH1
        assert_eq!(out_idx, channel_offset + CDF53_COEFFS_PER_CHANNEL);
    }

    output
}

/// CDF 5/3 lifting on a single row (or column) of length `n` (must be even).
/// Operates in-place. Lifting steps:
///   1. predict: x[2i+1] -= (x[2i] + x[2i+2]) / 2          (handle boundary with mirror)
///   2. update : x[2i]   += (x[2i-1] + x[2i+1] + 2) / 4   (mirror at boundaries)
/// After lifting, even indices hold low-pass (L) and odd indices hold high-pass (H).
/// We then re-arrange to [L_0..L_{n/2-1}, H_0..H_{n/2-1}] (deinterleave).
fn row_forward(buf: &mut [i32], n: usize) {
    // Predict step (odd indices become high-pass).
    for i in (1..n).step_by(2) {
        let left = buf[i - 1];
        let right = if i + 1 < n { buf[i + 1] } else { buf[i - 1] }; // mirror
        buf[i] -= (left + right) >> 1;
    }
    // Update step (even indices become low-pass).
    for i in (0..n).step_by(2) {
        let left = if i == 0 { buf[1] } else { buf[i - 1] };       // mirror
        let right = if i + 1 < n { buf[i + 1] } else { buf[i - 1] };// mirror
        buf[i] += (left + right + 2) >> 2;
    }
    // Deinterleave: L L L L H H H H
    let mut tmp = vec![0i32; n];
    let half = n / 2;
    for i in 0..half {
        tmp[i] = buf[2 * i];
        tmp[half + i] = buf[2 * i + 1];
    }
    buf[..n].copy_from_slice(&tmp);
}

fn col_forward(buf: &mut [i32], n: usize) {
    row_forward(buf, n);
}

/// Inverse CDF 5/3 lifting transform. Takes 3072 coefficients
/// (3 channels × 1024) and returns the recovered 32×32 BGR tile
/// as 3072 bytes (1024 pixels × 3 channels, no alpha).
pub fn inverse(coefficients: &[i16]) -> Vec<u8> {
    assert_eq!(coefficients.len(), CDF53_TOTAL_COEFFS);
    let mut output = vec![0u8; 32 * 32 * 3];

    for ch in 0..CDF53_CHANNELS {
        // Unpack channel layout back to work[][].
        let mut work = [[0i32; 32]; 32];
        let channel_offset = ch * CDF53_COEFFS_PER_CHANNEL;
        let mut in_idx = channel_offset;

        for y in 0..4 { for x in 0..4 { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }       // LL3
        for y in 0..4 { for x in 4..8 { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }       // HL3
        for y in 4..8 { for x in 0..4 { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }       // LH3
        for y in 4..8 { for x in 4..8 { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }       // HH3
        for y in 0..8  { for x in 8..16  { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }
        for y in 8..16 { for x in 0..8   { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }
        for y in 8..16 { for x in 8..16  { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }
        for y in 0..16  { for x in 16..32 { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }
        for y in 16..32 { for x in 0..16  { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }
        for y in 16..32 { for x in 16..32 { work[y][x] = coefficients[in_idx] as i32; in_idx += 1; } }
        assert_eq!(in_idx, channel_offset + CDF53_COEFFS_PER_CHANNEL);

        // 3 levels of 2D inverse decomposition.
        let mut size = 8;
        for _level in 0..3 {
            // Vertical inverse.
            for x in 0..size {
                let mut column = [0i32; 32];
                for y in 0..size {
                    column[y] = work[y][x];
                }
                col_inverse(&mut column, size);
                for y in 0..size {
                    work[y][x] = column[y];
                }
            }
            // Horizontal inverse.
            for y in 0..size {
                row_inverse(&mut work[y], size);
            }
            size *= 2;
        }

        // Pack to output[][] in BGR order with stride 3.
        for y in 0..32 {
            for x in 0..32 {
                let v = work[y][x].clamp(0, 255) as u8;
                output[(y * 32 + x) * 3 + ch] = v;
            }
        }
    }

    output
}

fn row_inverse(buf: &mut [i32], n: usize) {
    let half = n / 2;
    // Re-interleave: L0 H0 L1 H1 ...
    let mut tmp = vec![0i32; n];
    for i in 0..half {
        tmp[2 * i] = buf[i];
        tmp[2 * i + 1] = buf[half + i];
    }
    buf[..n].copy_from_slice(&tmp);
    // Inverse update.
    for i in (0..n).step_by(2) {
        let left = if i == 0 { buf[1] } else { buf[i - 1] };
        let right = if i + 1 < n { buf[i + 1] } else { buf[i - 1] };
        buf[i] -= (left + right + 2) >> 2;
    }
    // Inverse predict.
    for i in (1..n).step_by(2) {
        let left = buf[i - 1];
        let right = if i + 1 < n { buf[i + 1] } else { buf[i - 1] };
        buf[i] += (left + right) >> 1;
    }
}

fn col_inverse(buf: &mut [i32], n: usize) {
    row_inverse(buf, n);
}

/// Encode a tile's coefficients into 14 progressive bit-plane passes.
/// Each pass payload is the concatenation of 3 channels' RLE-encoded
/// bit-planes in BGR order:
///   [u16 BE len_B] [len_B bytes RLE bit-plane B]
///   [u16 BE len_G] [len_G bytes RLE bit-plane G]
///   [u16 BE len_R] [len_R bytes RLE bit-plane R]
/// Pass 0 = sign-bit-plane (bit set iff coefficient is negative).
/// Pass 1..13 = magnitude bit-planes from bit 12 down to bit 0.
pub fn encode_passes(coefficients: &[i16]) -> Vec<Vec<u8>> {
    assert_eq!(coefficients.len(), CDF53_TOTAL_COEFFS);
    let mut passes = Vec::with_capacity(CDF53_PASS_COUNT);
    for pass_idx in 0..CDF53_PASS_COUNT {
        let mut payload = Vec::new();
        for ch in 0..CDF53_CHANNELS {
            let channel_offset = ch * CDF53_COEFFS_PER_CHANNEL;
            let channel = &coefficients[channel_offset..channel_offset + CDF53_COEFFS_PER_CHANNEL];
            let bit_plane = extract_bit_plane(channel, pass_idx);
            let rle = rle_encode(&bit_plane);
            let len = rle.len() as u16;
            payload.push((len >> 8) as u8);
            payload.push(len as u8);
            payload.extend_from_slice(&rle);
        }
        passes.push(payload);
    }
    passes
}

/// Extract the bit-plane for the given pass index from a channel's coefficients.
/// Returns 128 bytes (1024 bits) — bit i corresponds to coefficient i.
/// pass_idx 0 = sign plane; pass_idx 1..=13 = magnitude bit 12..0.
fn extract_bit_plane(channel: &[i16], pass_idx: usize) -> Vec<u8> {
    debug_assert_eq!(channel.len(), CDF53_COEFFS_PER_CHANNEL);
    let mut out = vec![0u8; BIT_PLANE_BYTES_PER_CHANNEL];
    for (i, &coeff) in channel.iter().enumerate() {
        let bit = if pass_idx == 0 {
            // Sign plane.
            (coeff < 0) as u8
        } else {
            // Magnitude bit: pass 1 → bit 12 (MSB), pass 13 → bit 0 (LSB).
            let bit_pos = 13 - pass_idx;
            let magnitude = coeff.unsigned_abs() as u32;
            ((magnitude >> bit_pos) & 1) as u8
        };
        if bit != 0 {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

/// Run-length encode a byte buffer. Token format:
///   - 0x00..=0x7E: literal data byte.
///   - 0x7F: two-byte literal escape — next byte is the actual literal (used
///           when literal would be ≥ 0x80 which would otherwise collide with
///           the zero-run encoding).
///   - 0x80..=0xFF: zero run of (token & 0x7F) + 1 bytes (1..=128 zeros).
/// Optimized for sparse bit-planes where most bytes are zero.
fn rle_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0 {
            // Count run of zeros, up to 128.
            let mut run_len = 1;
            while i + run_len < data.len() && data[i + run_len] == 0 && run_len < 128 {
                run_len += 1;
            }
            out.push(0x80 | ((run_len - 1) as u8));
            i += run_len;
        } else if data[i] < 0x7F {
            out.push(data[i]);
            i += 1;
        } else {
            // 0x7F or 0x80..=0xFF: use escape token.
            out.push(0x7F);
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

fn rle_decode(rle: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rle.len() {
        let token = rle[i];
        if token == 0x7F {
            // Two-byte literal escape.
            if i + 1 < rle.len() {
                out.push(rle[i + 1]);
                i += 2;
            } else {
                break;
            }
        } else if token & 0x80 != 0 {
            // Zero run.
            let run_len = (token & 0x7F) as usize + 1;
            out.extend(std::iter::repeat(0u8).take(run_len));
            i += 1;
        } else {
            out.push(token);
            i += 1;
        }
    }
    out
}

/// Reassemble coefficients from N bit-plane passes (N ≤ CDF53_PASS_COUNT).
/// Truncated K < CDF53_PASS_COUNT yields a lossy approximation; passing all
/// CDF53_PASS_COUNT passes recovers the exact original coefficients.
pub fn decode_passes(passes: &[&[u8]]) -> Vec<i16> {
    assert!(passes.len() <= CDF53_PASS_COUNT);
    let mut coefficients = vec![0i16; CDF53_TOTAL_COEFFS];
    // Magnitude accumulation: u32 magnitude per coefficient.
    let mut magnitudes = vec![0u32; CDF53_TOTAL_COEFFS];
    // Sign tracking: separate from magnitude until final assembly.
    let mut signs = vec![false; CDF53_TOTAL_COEFFS];

    for (pass_idx, payload) in passes.iter().enumerate() {
        // Parse the per-pass payload (3 channels of RLE-encoded bit-planes).
        let mut offset = 0;
        for ch in 0..CDF53_CHANNELS {
            if offset + 2 > payload.len() { return coefficients; } // malformed; bail
            let len = ((payload[offset] as u16) << 8) | (payload[offset + 1] as u16);
            offset += 2;
            if offset + len as usize > payload.len() { return coefficients; }
            let rle = &payload[offset..offset + len as usize];
            offset += len as usize;
            let bit_plane = rle_decode(rle);
            let channel_offset = ch * CDF53_COEFFS_PER_CHANNEL;

            for i in 0..CDF53_COEFFS_PER_CHANNEL {
                let byte = bit_plane.get(i / 8).copied().unwrap_or(0);
                let bit = (byte >> (i % 8)) & 1;
                if bit != 0 {
                    if pass_idx == 0 {
                        signs[channel_offset + i] = true;
                    } else {
                        let bit_pos = 13 - pass_idx;
                        magnitudes[channel_offset + i] |= 1u32 << bit_pos;
                    }
                }
            }
        }
    }

    for i in 0..CDF53_TOTAL_COEFFS {
        let mag = magnitudes[i].min(i16::MAX as u32) as i16;
        coefficients[i] = if signs[i] { -mag } else { mag };
    }

    coefficients
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize a known BGRA 32×32 tile (alpha = 0xFF).
    pub(super) fn make_test_tile(seed: u32) -> Vec<u8> {
        let mut tile = vec![0u8; 32 * 32 * 4];
        let mut rng = seed;
        for px in tile.chunks_exact_mut(4) {
            // Lehmer LCG, deterministic.
            rng = rng.wrapping_mul(48271).wrapping_add(1);
            px[0] = (rng >> 8) as u8;
            px[1] = (rng >> 16) as u8;
            px[2] = (rng >> 24) as u8;
            px[3] = 0xFF;
        }
        tile
    }

    #[test]
    fn forward_inverse_roundtrip_exact() {
        let original = make_test_tile(0xDEAD_BEEF);
        let coefficients = forward(&original);
        assert_eq!(coefficients.len(), CDF53_TOTAL_COEFFS);
        let recovered = inverse(&coefficients);
        // Alpha is stripped, so compare BGR triplets only.
        for (orig_px, rec_px) in original.chunks_exact(4).zip(recovered.chunks_exact(3)) {
            assert_eq!(orig_px[0], rec_px[0], "B channel mismatch");
            assert_eq!(orig_px[1], rec_px[1], "G channel mismatch");
            assert_eq!(orig_px[2], rec_px[2], "R channel mismatch");
        }
    }
}

#[cfg(test)]
mod fixture {
    use super::*;
    use std::path::PathBuf;

    /// Regenerate `ghostframe-e2e/tests/fixtures/cdf53_fixture.json` from
    /// a deterministic seed. Cross-language ground truth — read by TS
    /// vitest (`prevalidate_cdf53.test.ts`) and WGSL e2e correctness
    /// tests. Gated `#[ignore]` so a normal `cargo test` doesn't rewrite
    /// the committed file; run explicitly via:
    ///
    ///   cargo test -p ghostframe-lib fixture::write_reference_fixture \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore = "fixture writer — run explicitly when the codec changes"]
    fn write_reference_fixture() {
        let pixels = super::tests::make_test_tile(0xCDF5_3B);
        let coefficients = forward(&pixels);
        let passes = encode_passes(&coefficients);

        // Pre-RLE bit-planes per pass per channel — what the client's
        // GPU integrate shader sees after CPU rleDecode.
        let mut bit_planes_per_pass: Vec<Vec<Vec<u8>>> = Vec::with_capacity(CDF53_PASS_COUNT);
        for pass_idx in 0..CDF53_PASS_COUNT {
            let mut per_channel: Vec<Vec<u8>> = Vec::with_capacity(CDF53_CHANNELS);
            for ch in 0..CDF53_CHANNELS {
                let off = ch * CDF53_COEFFS_PER_CHANNEL;
                let channel = &coefficients[off..off + CDF53_COEFFS_PER_CHANNEL];
                per_channel.push(extract_bit_plane(channel, pass_idx));
            }
            bit_planes_per_pass.push(per_channel);
        }

        let mut json = String::new();
        json.push_str("{\n");
        json.push_str(&format!("  \"seed\": \"0xCDF5_3B\",\n"));
        json.push_str(&format!("  \"tile_size\": 32,\n"));
        json.push_str(&format!("  \"channels\": 3,\n"));
        json.push_str(&format!("  \"coefficients_per_channel\": {},\n", CDF53_COEFFS_PER_CHANNEL));
        json.push_str(&format!("  \"pass_count\": {},\n", CDF53_PASS_COUNT));
        // pixels (BGRA, 4096 bytes)
        json.push_str("  \"pixels_bgra\": [");
        for (i, b) in pixels.iter().enumerate() {
            if i > 0 { json.push(','); }
            json.push_str(&b.to_string());
        }
        json.push_str("],\n");
        // expected_coefficients (3072 i16 values, decimal)
        json.push_str("  \"expected_coefficients\": [");
        for (i, c) in coefficients.iter().enumerate() {
            if i > 0 { json.push(','); }
            json.push_str(&c.to_string());
        }
        json.push_str("],\n");
        // encoded_passes (14 entries, each is the raw RLE-encoded payload bytes)
        json.push_str("  \"encoded_passes\": [\n");
        for (i, pass) in passes.iter().enumerate() {
            if i > 0 { json.push_str(",\n"); }
            json.push_str("    [");
            for (j, b) in pass.iter().enumerate() {
                if j > 0 { json.push(','); }
                json.push_str(&b.to_string());
            }
            json.push(']');
        }
        json.push_str("\n  ],\n");
        // bit_planes_per_pass (14 passes × 3 channels × 128 bytes each, pre-RLE)
        json.push_str("  \"bit_planes_per_pass\": [\n");
        for (i, pass) in bit_planes_per_pass.iter().enumerate() {
            if i > 0 { json.push_str(",\n"); }
            json.push_str("    [\n");
            for (j, ch) in pass.iter().enumerate() {
                if j > 0 { json.push_str(",\n"); }
                json.push_str("      [");
                for (k, b) in ch.iter().enumerate() {
                    if k > 0 { json.push(','); }
                    json.push_str(&b.to_string());
                }
                json.push(']');
            }
            json.push_str("\n    ]");
        }
        json.push_str("\n  ]\n");
        json.push_str("}\n");

        // Resolve workspace root via CARGO_MANIFEST_DIR.
        let path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().expect("workspace root")
            .join("ghostframe-e2e/tests/fixtures/cdf53_fixture.json");
        std::fs::create_dir_all(path.parent().unwrap())
            .expect("create fixtures dir");
        std::fs::write(&path, json).expect("write fixture");
        eprintln!("wrote fixture: {}", path.display());
    }
}

#[cfg(test)]
mod tests_passes {
    use super::*;

    #[test]
    fn encode_decode_passes_roundtrip() {
        let original = super::tests::make_test_tile(0x1234_5678);
        let coefficients = forward(&original);
        let passes = encode_passes(&coefficients);
        assert_eq!(passes.len(), CDF53_PASS_COUNT);
        let pass_refs: Vec<&[u8]> = passes.iter().map(|p| p.as_slice()).collect();
        let recovered = decode_passes(&pass_refs);
        assert_eq!(recovered, coefficients,
            "encode_passes → decode_passes should recover coefficients exactly");
    }

    #[test]
    fn rle_roundtrip() {
        for seed in [0u32, 0xCAFE_BABE, 0xDEAD_BEEF, 0xFFFF_FFFF] {
            let mut data = vec![0u8; 128];
            let mut rng = seed;
            for b in data.iter_mut() {
                rng = rng.wrapping_mul(48271).wrapping_add(1);
                // Bias toward zeros (75% zero) to exercise zero runs.
                *b = if (rng >> 24) < 64 { (rng >> 16) as u8 } else { 0 };
            }
            let encoded = rle_encode(&data);
            let decoded = rle_decode(&encoded);
            assert_eq!(decoded, data, "RLE roundtrip failed for seed={seed:08x}");
        }
    }

    /// Truncating passes (dropping bottom K bit-planes) yields a lossy
    /// approximation whose distance from the original is monotone non-decreasing
    /// in K. Verified via sum-of-squared-error (proxy for SSIM ordering).
    #[test]
    fn truncated_pass_error_monotone() {
        let original = super::tests::make_test_tile(0xAAAA_5555);
        let coefficients = forward(&original);
        let passes = encode_passes(&coefficients);

        let mut prev_error: Option<u64> = None;
        // Iterate K from full (14) down to 1 — dropping more passes ⇒ more error.
        for k in (1..=CDF53_PASS_COUNT).rev() {
            let pass_refs: Vec<&[u8]> = passes.iter().take(k).map(|p| p.as_slice()).collect();
            let recovered_coeffs = decode_passes(&pass_refs);
            let recovered_pixels = inverse(&recovered_coeffs);
            let mut error: u64 = 0;
            for (orig_px, rec_px) in original.chunks_exact(4).zip(recovered_pixels.chunks_exact(3)) {
                for ch in 0..3 {
                    let d = orig_px[ch] as i32 - rec_px[ch] as i32;
                    error += (d * d) as u64;
                }
            }
            if let Some(prev) = prev_error {
                assert!(error >= prev,
                    "error monotonicity violated: k={k} error={error} < prev_error={prev}");
            }
            prev_error = Some(error);
            // At full passes (k=14), error must be zero.
            if k == CDF53_PASS_COUNT {
                assert_eq!(error, 0, "k={k} (all passes) should yield zero error; got {error}");
            }
        }
    }
}
