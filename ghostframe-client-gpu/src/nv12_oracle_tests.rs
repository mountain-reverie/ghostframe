//! Oracle 2 (Task 8): the shipped `h264_nv12_blit.wgsl` against the CPU
//! reference in `nv12_reference.rs`, in the two exact tiers design doc
//! §9.1-9.3 measured this GPU actually needs -- not the plan's original
//! single-tier "assert equal" oracle, which predates that measurement.
//!
//! **Tier A** renders to `Rgba32Float` and asserts f32 bit-equality against
//! [`crate::nv12_reference::nv12_pixel_to_rgb_f32`]: that format receives
//! the fragment shader's `vec4<f32>` untouched (no fp16 packing -- §9.1), so
//! this is the arithmetic gate, at full precision.
//!
//! **Tier B** renders to a real `Rgba8Unorm` framebuffer. On AMD it asserts
//! equality against `round_half_away(f16_rtz(v) * 255)` -- the measured
//! model of this GPU's compressed fp16 ABGR export path (§9.1). On other
//! vendors, §9.2 says to relax to the two-element set `{round(v),
//! round(f16_rtz(v))}` (adjacent by construction, never a magnitude
//! tolerance) and let Tier A carry arithmetic correctness -- see
//! `is_amd`/`round_conformant` below. That quantiser is `gpu_byte`/
//! `f16_rtz_bits`, deliberately NOT in `nv12_reference.rs`, which models a
//! conformant write, not this specific hardware quirk.
//!
//! ## Why this lives in `src/`, not `tests/*.rs`
//!
//! Every other GPU test in this crate (`tests/gpu_export.rs`,
//! `gpu_pipelines.rs`, `gpu_oracle.rs`, `gpu_import.rs`) is a separate crate
//! unit that hard-fails with `.expect(...)` and is simply never named in any
//! CI workflow -- CI exempts itself in the workflow file, per this
//! project's rule (see `gpu_import.rs`'s module doc, and the `feedback_
//! ci_exempts_itself` precedent this mirrors). That works there because
//! none of those files need anything from this crate gated behind the
//! `test-support` feature.
//!
//! This oracle does: it needs [`crate::nv12_reference`], which is
//! `#[cfg(any(test, feature = "test-support"))]`. A `tests/*.rs` file is a
//! separate crate unit that never sees `cfg(test)` from this crate's own
//! build, so reaching `nv12_reference` from there needs `test-support`
//! active for that specific build -- and the only ways to get a cargo
//! feature active without an explicit `--features` flag are (a) making it a
//! default feature, which ships `nv12_reference` in every release build of
//! every crate that forgets `default-features = false` (rejected already,
//! twice, for the identical shape in `ghostframe-client-h264`; see this
//! crate's own `Cargo.toml`), or (b) a self-referencing dev-dependency,
//! which compiles this crate twice and makes `crate::GpuError` and
//! `ghostframe_client_gpu::GpuError` distinct types (rejected earlier in
//! this same milestone). Neither is acceptable, so -- exactly like
//! `ghostframe-client-h264`'s own `oracle_tests.rs`, which hit the identical
//! problem with its `testclip`/`test-support` -- this is a `#[cfg(test)]`
//! module in `src/`, which sees `nv12_reference` with no feature and no
//! extra dependency at all.
//!
//! The cost of that choice: unlike its `tests/*.rs` siblings, this module
//! DOES get built and run by `cargo test --workspace --lib`
//! (`ci.yml:71`), on a runner with no GPU. So unlike those siblings it
//! cannot hard-fail on a missing device -- it has two audiences at once (a
//! developer's own GPU machine, where a real failure must be loud, and a
//! GPU-less CI runner, where "no device" is simply the expected case) and
//! self-skips on `WgpuContext::new()` failing, same idiom the design plan's
//! own Task 8 text used before this file existed. That does mean a
//! genuinely broken `WgpuContext::new()` would read as "skipped" here rather
//! than "failed" -- the risk `gpu_import.rs` explicitly rejected self-skip
//! over. Accepted specifically because every *other* GPU test in this crate
//! already covers that hard-fail case from `tests/*.rs`; this module's job
//! is the shader/reference agreement, not re-proving `WgpuContext::new()`
//! itself.

#[cfg(test)]
mod tests {
    use crate::framebuffer::Framebuffer;
    use crate::nv12_reference::nv12_pixel_to_rgb_f32;
    use crate::pipelines::h264_nv12::H264Nv12Pipeline;
    use crate::wgpu_ctx::WgpuContext;

    /// Kept inside `wgpu::Limits::downlevel_defaults()`'s 2048 max texture
    /// dimension (`WgpuContext::new` requests exactly those limits, bumping
    /// only `max_storage_buffers_per_shader_stage`) -- `2048` itself is the
    /// documented maximum, not merely "under" it.
    const W: u32 = 2048;
    const H: u32 = 2048;

    /// Width, in luma columns, of each of the two chroma-exhaustive edge
    /// bands in [`synthetic_nv12`] -- see that function's doc for what these
    /// buy.
    const EDGE_BAND: u32 = 512;

    /// Synthetic NV12 planes built to sweep, not to coincidentally pass.
    ///
    /// The image is three vertical bands:
    ///
    /// - `x < EDGE_BAND`: luma forced to `0` on every pixel.
    /// - `x >= W - EDGE_BAND`: luma forced to `255` on every pixel.
    /// - the `1024`-column middle: luma `(x + 3*y) % 256`, decorrelated from
    ///   the chroma index so the two don't move in lockstep.
    ///
    /// Chroma is `cb = cx % 256, cr = cy % 256` (`cx`/`cy` the chroma-plane
    /// block index) -- independent per axis, so both vary over their full
    /// range without walking the plane in lockstep. Measured, not assumed
    /// (`python3` sweep over this exact construction, kept in the commit
    /// history for this file): this reaches all 65,536 `(cb, cr)` pairs
    /// overall, AND all 65,536 pairs *within each edge band on its own* --
    /// each `EDGE_BAND`-wide band is `256` chroma columns wide, enough on
    /// its own for `cb = cx % 256` (or, mirrored, `cr = cy % 256` against
    /// the full image height) to hit every residue, so `Y=0` and `Y=255`
    /// are each tested against the *entire* chroma plane, not a slice of
    /// it. The full grid (edge bands plus the decorrelated middle) yields
    /// 391,168 distinct `(Y, cb, cr)` triples -- out of 16,777,216 possible,
    /// far short of the full cube, but roughly 12x what the version of this
    /// sweep an earlier review measured (33,272), at the same pixel count
    /// order of magnitude.
    ///
    /// An earlier revision of this function claimed the edges were tested
    /// "against the entire chroma sweep" while actually forcing a single
    /// edge *column*, which pairs `Y=0`/`Y=255` against only the chroma
    /// values reachable from `cx = 0` (or `cx = (W-1)/2`) -- a thin slice,
    /// not the full plane. The band construction above is what makes the
    /// comment true rather than aspirational.
    fn synthetic_nv12() -> (Vec<u8>, Vec<u8>) {
        let mut luma = vec![0u8; (W * H) as usize];
        for y in 0..H {
            for x in 0..W {
                let v = if x < EDGE_BAND {
                    0
                } else if x >= W - EDGE_BAND {
                    255
                } else {
                    ((x.wrapping_add(3 * y)) % 256) as u8
                };
                luma[(y * W + x) as usize] = v;
            }
        }

        let cw = W / 2;
        let ch = H / 2;
        let mut chroma = vec![0u8; (cw * ch * 2) as usize];
        for cy in 0..ch {
            for cx in 0..cw {
                let cb = (cx % 256) as u8;
                let cr = (cy % 256) as u8;
                let idx = ((cy * cw + cx) * 2) as usize;
                chroma[idx] = cb;
                chroma[idx + 1] = cr;
            }
        }
        (luma, chroma)
    }

    /// Chroma sample selected for luma pixel `(x, y)`, matching the shader's
    /// `p / 2` nearest-neighbour indexing.
    fn chroma_at(chroma: &[u8], x: u32, y: u32) -> (u8, u8) {
        let cw = W / 2;
        let idx = (((y / 2) * cw + (x / 2)) * 2) as usize;
        (chroma[idx], chroma[idx + 1])
    }

    /// True when `ctx`'s adapter is AMD -- the only vendor `gpu_byte`'s
    /// fp16-RTZ compressed-export model (design doc §9.1) is measured
    /// against. `0x1002` is AMD's PCI vendor ID; `WgpuContext` only enables
    /// the Vulkan backend, so `AdapterInfo::vendor` is always a real PCI
    /// vendor ID here, not a backend-specific substitute (see that field's
    /// own doc in `wgpu-types`).
    fn is_amd(ctx: &WgpuContext) -> bool {
        const AMD_PCI_VENDOR_ID: u32 = 0x1002;
        ctx.adapter.get_info().vendor == AMD_PCI_VENDOR_ID
    }

    /// Read a whole `Rgba32Float` texture back as `[f32; 4]` per pixel.
    fn read_rgba32f(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture) -> Vec<f32> {
        let unpadded_bytes_per_row = W * 16; // 4 x f32
        let padded_bytes_per_row = unpadded_bytes_per_row
            .div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12-oracle-tier-a-readback"),
            size: padded_bytes_per_row as u64 * H as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("nv12-oracle-tier-a-readback"),
        });
        encoder.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(H),
                },
            },
            wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| {
            r.expect("map tier A staging buffer")
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll tier A readback");

        let padded = slice.get_mapped_range().expect("get_mapped_range");
        let mut tight = Vec::with_capacity((W * H * 4) as usize);
        for row in 0..H as usize {
            let start = row * padded_bytes_per_row as usize;
            let row_bytes = &padded[start..start + unpadded_bytes_per_row as usize];
            for chunk in row_bytes.chunks_exact(4) {
                tight.push(f32::from_le_bytes(chunk.try_into().expect("4 bytes")));
            }
        }
        tight
    }

    #[test]
    fn tier_a_matches_the_cpu_reference_bit_exact_on_rgba32float() {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("no usable GPU; skipping tier A NV12 oracle");
            return;
        };

        let (luma, chroma) = synthetic_nv12();
        let mut pipeline =
            H264Nv12Pipeline::with_format(&ctx.device, wgpu::TextureFormat::Rgba32Float);
        let (luma_tex, chroma_tex) =
            H264Nv12Pipeline::upload_planes(&ctx.device, &ctx.queue, W, H, &luma, &chroma);

        let target = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-oracle-tier-a-target"),
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        pipeline.draw_to_texture(&ctx.device, &ctx.queue, &target, &luma_tex, &chroma_tex);
        let got = read_rgba32f(&ctx.device, &ctx.queue, &target);

        let mut compared = 0usize;
        let mut mismatches = 0usize;
        let mut first_bad = None;
        for y in 0..H {
            for x in 0..W {
                let (cb, cr) = chroma_at(&chroma, x, y);
                let want = nv12_pixel_to_rgb_f32(luma[(y * W + x) as usize], cb, cr);
                let base = ((y * W + x) * 4) as usize;
                let have = [got[base], got[base + 1], got[base + 2]];
                for c in 0..3 {
                    compared += 1;
                    if have[c].to_bits() != want[c].to_bits() {
                        mismatches += 1;
                        if first_bad.is_none() {
                            first_bad = Some((x, y, c, want[c], have[c]));
                        }
                    }
                }
            }
        }

        eprintln!("tier A: {compared} channel-samples compared, {mismatches} differed");
        assert_eq!(
            mismatches, 0,
            "{mismatches} of {compared} channel-samples differ bit-exactly from the CPU \
             pre-quantisation reference on an Rgba32Float target (Tier A, design doc §9.2). \
             First mismatch (x, y, channel, want, have): {first_bad:?}. This tier isolates \
             arithmetic from the AMD fp16-pack write path (no packing occurs for this \
             format), so any difference here is a real bug in the shader or the reference, \
             not a hardware rounding quirk -- investigate before considering any tolerance."
        );
    }

    #[test]
    fn tier_b_matches_the_measured_export_model_exact_on_rgba8unorm() {
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("no usable GPU; skipping tier B NV12 oracle");
            return;
        };
        let amd = is_amd(&ctx);

        let (luma, chroma) = synthetic_nv12();
        let mut pipeline = H264Nv12Pipeline::new(&ctx.device);
        let fb = Framebuffer::new(&ctx.device, W, H);
        let (luma_tex, chroma_tex) =
            H264Nv12Pipeline::upload_planes(&ctx.device, &ctx.queue, W, H, &luma, &chroma);

        pipeline.draw(&ctx.device, &ctx.queue, &fb, &luma_tex, &chroma_tex);
        let got = fb.debug_read(&ctx.device, &ctx.queue);

        let mut compared = 0usize;
        let mut mismatches = 0usize;
        let mut first_bad = None;
        for y in 0..H {
            for x in 0..W {
                let (cb, cr) = chroma_at(&chroma, x, y);
                let want_f32 = nv12_pixel_to_rgb_f32(luma[(y * W + x) as usize], cb, cr);
                let base = ((y * W + x) * 4) as usize;
                for c in 0..3 {
                    compared += 1;
                    let exact = gpu_byte(want_f32[c]);
                    let have = got[base + c];
                    // §9.2: on AMD, the fp16-RTZ model is exact, no slack.
                    // On any other vendor, relax to the two-element accept
                    // set {conformant round, fp16-RTZ round} -- both are
                    // exact values, adjacent by construction (RTZ can only
                    // round the same way or one step closer to zero), never
                    // a magnitude tolerance -- and let Tier A alone carry
                    // arithmetic correctness there.
                    let ok = if amd {
                        have == exact
                    } else {
                        have == exact || have == round_conformant(want_f32[c])
                    };
                    if !ok {
                        mismatches += 1;
                        if first_bad.is_none() {
                            first_bad = Some((x, y, c, exact, have));
                        }
                    }
                }
            }
        }
        // Alpha is a constant 1.0 write in this shader on every pixel, so it
        // is checked separately rather than inflating the "channel-samples
        // compared" count the way it would not in design doc §9.2's own
        // 50,331,648 = 256^3 * 3 (RGB only).
        for y in [0, H / 2, H - 1] {
            for x in [0, W / 2, W - 1] {
                let base = ((y * W + x) * 4) as usize;
                assert_eq!(got[base + 3], 255, "alpha must be opaque at ({x},{y})");
            }
        }

        eprintln!(
            "tier B ({}): {compared} channel-samples compared, {mismatches} differed",
            if amd {
                "AMD, exact"
            } else {
                "non-AMD, relaxed per §9.2"
            }
        );
        assert_eq!(
            mismatches,
            0,
            "{mismatches} of {compared} channel-samples differ from the measured export model \
             (Tier B, design doc §9.1-9.2; {} on this adapter). First mismatch (x, y, channel, \
             want, have): {first_bad:?}.",
            if amd {
                "round_half_away(f16_rtz(v) * 255), exact"
            } else {
                "{round(v), round(f16_rtz(v))}, relaxed for non-AMD"
            }
        );
    }

    /// Model of AMD's compressed fp16 ABGR export path for an `Rgba8Unorm`
    /// render target on GCN/Polaris (ACO's `v_cvt_pkrtz_f16_f32`, confirmed
    /// at the ISA level -- design doc §9.1): the fragment shader's clamped
    /// `[0, 1]` channel value is packed to IEEE754 binary16 with ROUND
    /// TOWARD ZERO before the ROP's ordinary round-half-away-from-zero
    /// unorm8 write.
    ///
    /// Lives here, in the test, not in `nv12_reference.rs`: that file models
    /// a conformant write; this models one specific GPU's compressed-export
    /// quirk, which is exactly the distinction design doc §9.2 draws between
    /// the two tiers.
    fn gpu_byte(v: f32) -> u8 {
        let packed = f16_bits_to_f32(f32_to_f16_rtz_bits(v));
        round_conformant(packed)
    }

    /// The conformant unorm8 write: round the exact product to the nearest
    /// integer, ties away from zero, with no fp16 packing at all. Same
    /// formula as `nv12_reference::to_u8` (private to that module, so
    /// duplicated here rather than exposed) -- the non-AMD half of §9.2's
    /// two-element accept set, and also `gpu_byte`'s own final rounding
    /// step once its input has already been through the fp16-RTZ pack.
    fn round_conformant(v: f32) -> u8 {
        (v.clamp(0.0, 1.0) as f64 * 255.0 + 0.5).floor() as u8
    }

    /// `f32 -> binary16`, rounding toward zero (truncation), not the
    /// round-to-nearest-even every standard library/hardware `f16`
    /// conversion normally performs.
    ///
    /// Domain reached from `gpu_byte` is always `[0, 1]` (post-`clamp`), and
    /// within THAT domain this is implemented generally (subnormal results,
    /// zero) and exercised over a broad sweep by the property tests below,
    /// not just a handful of points. Outside that domain it is only
    /// half-general: overflow (`unbiased_exp > 15`) saturates toward the
    /// largest finite value, which is the correct RTZ answer for a large
    /// finite input, but a real f32 infinity or NaN also lands in that same
    /// branch and is wrongly turned into a finite value instead of f16
    /// infinity/NaN. Unreachable from this shader's `[0, 1]` domain, so left
    /// as is rather than special-cased for inputs this function never sees.
    fn f32_to_f16_rtz_bits(v: f32) -> u16 {
        let bits = v.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        if v == 0.0 {
            return sign;
        }

        let abs_bits = bits & 0x7FFF_FFFF;
        let exp_f32 = ((abs_bits >> 23) & 0xFF) as i32;
        let mantissa_f32 = abs_bits & 0x007F_FFFF;

        if exp_f32 == 0 {
            // f32 subnormal (< 2^-126): already far below the smallest f16
            // magnitude (2^-24), so RTZ truncates to (signed) zero.
            return sign;
        }

        let unbiased_exp = exp_f32 - 127;

        if unbiased_exp > 15 {
            // Overflow: RTZ saturates to the largest finite f16 magnitude
            // rather than going to infinity. Not reachable from this
            // shader's [0, 1] domain (max unbiased_exp there is 0) -- see
            // this function's doc for why real infinity/NaN inputs would
            // land here too, and wrongly.
            return sign | 0x7BFF;
        }
        if unbiased_exp < -24 {
            // Too small even for an f16 subnormal: truncates to zero.
            return sign;
        }

        let full_mantissa = (1u32 << 23) | mantissa_f32; // 24-bit significand, implicit leading 1.

        if unbiased_exp < -14 {
            // f16 subnormal result.
            let shift = (-14 - unbiased_exp) as u32 + 13;
            let sub_mantissa = if shift >= 32 {
                0
            } else {
                full_mantissa >> shift
            };
            return sign | (sub_mantissa as u16 & 0x3FF);
        }

        // f16 normal result: truncate (not round) the low 13 mantissa bits.
        let f16_exp = (unbiased_exp + 15) as u16;
        let f16_mantissa = (mantissa_f32 >> 13) as u16;
        sign | (f16_exp << 10) | f16_mantissa
    }

    /// Exact `binary16 -> f32` widening (every f16 value is exactly
    /// representable in f32, so this has no rounding of its own).
    fn f16_bits_to_f32(bits: u16) -> f32 {
        let sign = ((bits & 0x8000) as u32) << 16;
        let exp = (bits >> 10) & 0x1F;
        let mantissa = (bits & 0x3FF) as u32;

        if exp == 0 {
            if mantissa == 0 {
                return f32::from_bits(sign);
            }
            let mut m = mantissa;
            let mut e: i32 = -14;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x3FF;
            let f32_exp = (e + 127) as u32;
            return f32::from_bits(sign | (f32_exp << 23) | (m << 13));
        }
        if exp == 0x1F {
            return f32::from_bits(sign | (0xFFu32 << 23) | (mantissa << 13));
        }
        let f32_exp = (exp as i32 - 15 + 127) as u32;
        f32::from_bits(sign | (f32_exp << 23) | (mantissa << 13))
    }

    #[test]
    fn f16_rtz_round_trips_exactly_representable_values() {
        for exact in [0.0f32, 0.5, 1.0, 0.25, 0.75, 0.125, 0.0625] {
            assert_eq!(
                f16_bits_to_f32(f32_to_f16_rtz_bits(exact)),
                exact,
                "f16 exactly represents {exact}; RTZ must round-trip it unchanged"
            );
        }
    }

    #[test]
    fn f16_rtz_bit_pattern_matches_the_standard_layout_for_exact_values() {
        // Round-tripping through THIS module's own encoder/decoder pair
        // (the test above) cannot catch a self-consistent layout bug -- a
        // truncator shaped like bfloat16 (1-8-7 sign/exponent/mantissa
        // instead of binary16's 1-5-10) would round-trip through its own
        // matching decoder just fine, and would also satisfy every property
        // test below (never overshoots, never rounds further than
        // round-to-nearest): those properties hold for ANY truncating
        // float format, not specifically binary16.
        //
        // For a value that already sits exactly on the f16 grid, RTZ and
        // round-to-nearest cannot differ -- no rounding decision is even
        // reachable -- so comparing raw bits against `half`'s independent
        // binary16 encoder pins the exact layout (5-bit exponent, 10-bit
        // mantissa, 13-bit truncation shift from f32's 23-bit mantissa).
        for exact in [
            0.0f32,
            1.0,
            0.5,
            0.25,
            2.0,
            3.0,
            1.5,
            1.0 + 2f32.powi(-10), // smallest f16 step above 1.0.
            1.0 - 2f32.powi(-11), // largest f16 value strictly below 1.0.
        ] {
            let want = half::f16::from_f32(exact).to_bits();
            let got = f32_to_f16_rtz_bits(exact);
            assert_eq!(
                got, want,
                "exact value {exact} needs no rounding decision, so RTZ and round-to-nearest \
                 must produce identical bits: got {got:#06x}, want {want:#06x}"
            );
        }
    }

    #[test]
    fn f16_rtz_is_never_further_from_zero_than_round_to_nearest() {
        // Property check, not the oracle itself: for every value in a dense
        // sweep of [0, 1], truncation must (a) never overshoot the true
        // value, since "round toward zero" on a non-negative number is
        // exactly "round down", and (b) never land further from zero than
        // round-to-nearest-even would, since the only way RTZ and
        // round-nearest can differ is nearest rounding UP across a boundary
        // that RTZ stays below.
        for i in 0..=10_000u32 {
            let v = i as f32 / 10_000.0;
            let rtz = f16_bits_to_f32(f32_to_f16_rtz_bits(v));
            assert!(rtz <= v, "RTZ({v}) = {rtz} exceeds the true value");
            let nearest = half::f16::from_f32(v).to_f32();
            assert!(
                rtz <= nearest,
                "RTZ must never land further from zero than round-to-nearest: \
                 v={v} rtz={rtz} nearest={nearest}"
            );
        }
    }

    #[test]
    fn f16_rtz_truncates_a_tie_downward_where_round_to_nearest_rounds_up() {
        // 1.0 - 2^-12, exactly halfway between the two f16 grid points
        // (1.0 - 2^-11) and 1.0 in the [0.5, 1) binade (f16 ULP there is
        // 2^-11). Round-to-nearest-even resolves the tie up, to 1.0 (whose
        // mantissa is even); round-TOWARD-ZERO must not, on any tie,
        // wherever it falls.
        let v = 1.0f32 - 2f32.powi(-12);
        let rtz = f16_bits_to_f32(f32_to_f16_rtz_bits(v));
        let nearest = half::f16::from_f32(v).to_f32();
        assert_eq!(
            nearest, 1.0,
            "test premise: round-to-nearest resolves this tie up to 1.0"
        );
        assert!(
            rtz < 1.0,
            "RTZ of a value below 1.0 must never round up to 1.0, got {rtz}"
        );
    }
}
