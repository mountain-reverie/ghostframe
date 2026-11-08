# CDF 5/3 encode entirely on the GPU (PR 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Encode CDF 5/3 tiles entirely on the server GPU: forward, bit-planes, presence, RLE and packing. Only the wire payload and a 48-byte record per tile come back, and `encode_passes_sparse` no longer runs on the CPU in production.

**Architecture:** `cdf53_encode.comp` from PR 1 gains two encode variants:
- `_sg`: 32-lane subgroup scans;
- `_wg`: workgroup-memory scans.

The device picks one at start-up after a self-test against the CPU encoder. Each of the three tile lists (the frame's dirty list, escalation, fallback) gets a payload buffer, a record buffer and a bump counter (`Cdf53Out`). The host validates each record and builds the existing `Cdf53Encoded`, so the emitter and scheduler are unchanged. The coefficient buffers go, except the dirty one under the `cdf53-diag` feature.

**Tech Stack:** Rust (ash 0.38, Vulkan 1.1 + optional `VK_EXT_subgroup_size_control`), GLSL 450 via `glslc --target-env=vulkan1.1`, cargo tests through `~/work/gf-gpu-run`.

**Spec:** `docs/superpowers/specs/2026-11-07-cdf53-gpu-encode-design.md`. Task 10 updates two points where this plan settles what the spec left open (sections 4.4 and 6).

**Prerequisite:** PR 1 (`docs/superpowers/plans/2026-11-07-cdf53-gpu-forward-pr1.md`) merged, or this branch stacked on it. Everything below assumes PR 1's code:
- `cdf53_encode.comp` with the forward-only variant;
- `cdf53_shader_gen.rs`;
- the `shaders-cdf53` recipe;
- `run_cdf53_forward_stage`;
- `cdf53_forward_gate_tests.rs`.

**The verified shader:** the complete PR 2 shader source has already passed on the RX 480, byte-exact against the CPU encoder:
- both RLE variants and the forward, on 1080p and 4K noise / photo / text and 32 adversarial tiles;
- a capacity overflow with a canary;
- a mutation check.

It is `ghostframe-lib/src/capture/shaders/spike/cdf53_encode_pr2.comp` on branch `spike/cdf53-gpu` (commit c11b0715; gate `cdf53_plan_shader_gate` in that branch's `cdf53_spike.rs`). Task 1 copies it; do not retype it.

---

## Ground rules

Same as PR 1's plan, "Ground rules":
- a `gf-*` worktree, branch `perf/cdf53-gpu-encode` from `origin/master` with PR 1 merged (or from `perf/cdf53-forward-parallel` if not);
- copy the web-client `dist/` into it;
- GPU only through `~/work/gf-gpu-run`, one at a time, and the kernel-log check after each run;
- explicit `git add`, never push, `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`;
- never kill processes; disk limits.

Run `cargo fmt --all` before every commit: the plan's Rust is not guaranteed rustfmt-clean, and CI's fmt job checks the tip. Every task ends with `cargo clippy --workspace --all-targets -- -D warnings` and `cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings` clean. A non-compiling test target vanishes from `cargo test`'s summary, so clippy is the gate.

## Decisions this plan makes (the spec left them open)

1. **No variant passes its self-test.** `GpuFrameProcessor` construction fails, and the daemon refuses to start with the reason, as it already does for a host with no usable GPU (`require_gpu_backend`). A device that cannot run this shader correctly cannot be trusted with the rest of the pipeline either.
2. **A refused record** (owner decision 2026-11-07): the tile is treated as a new tile. It is encoded again from this frame's pixels through the fallback encode (`encode_cdf53_tiles`) and emitted in this frame, in the list it came from (dirty or escalation). Nothing is dropped, and nothing outside that tile is repainted. If that re-encode fails too, the GPU baseline is invalidated (`invalidate_baseline(1)`) so the next frame marks every tile dirty and resends it. Leaving it out would strand it, because the SAD stage has already committed it to the reference. A fallback batch keeps today's behaviour: the call fails, and the caller logs, invalidates and retries.
3. **The payload is assembled in workgroup memory and copied out as whole words.** The production device enables no 8-bit storage, so the spike's byte stores are gone. Tile offsets are 4-byte aligned, so word stores suffice.
4. **The forward pipeline stays built in every build.** It is small. It is dispatched only under `cdf53-diag`, into the dirty coefficient buffer, which exists only under that feature. The descriptor pool always counts it, so pool sizes do not depend on features.
5. **`Cdf53Encoded` moves** from `transport::io_bridge::frame_backend` to `capture::gpu_pipeline::cdf53_encode`, re-exported from `frame_backend`, so no consumer changes its import.

## File map

| File | Change |
|---|---|
| `ghostframe-lib/src/capture/shaders/cdf53_encode.comp` | encode variants (copied from the spike) |
| `ghostframe-lib/src/capture/shaders/cdf53_encode_{sg,wg}.spv` | **new** builds |
| `ghostframe-lib/src/capture/gpu_pipeline/cdf53_encode.rs` | **new**: `Cdf53Encoded`, records, `decode_record`, subgroup planning, self-test tiles |
| `ghostframe-lib/src/capture/gpu_pipeline/cdf53_shader_gen.rs` | constants shared with the shader are checked |
| `ghostframe-lib/src/capture/pipeline_builder.rs` | `build_compute_pipeline_sized` (required subgroup size) |
| `ghostframe-lib/src/config.rs` | `Cdf53RleChoice`, `TransportConfig::cdf53_rle` (`GHOSTFRAME_CDF53_RLE`) |
| `ghostframe-lib/src/capture/gpu_pipeline/setup.rs` | subgroup probe, extension, encode pipelines, `Cdf53Out` buffers, pool |
| `ghostframe-lib/src/capture/gpu_pipeline/mod.rs` | fields, `Cdf53Out`, `with_cdf53_rle`, self-test, drop |
| `ghostframe-lib/src/capture/gpu_pipeline/frame.rs` | `run_cdf53_encode_stage`; dirty / escalation / fallback encode; diag forward |
| `ghostframe-lib/src/transport/io_bridge/frame_backend.rs` | records → `Cdf53Encoded`; trait `encode_cdf53_tiles`; timings |
| `ghostframe-lib/src/transport/io_bridge/{frame_worker,emit,frame}.rs`, `unchanged_oracle_tests.rs` | rename and types |
| `ghostframe-lib/src/transport/io_bridge.rs` | construct with `with_cdf53_rle` |
| `ghostframe-lib/src/capture/gpu_pipeline/{tests,damage_tests,fallback_tests,readback_bench}.rs` | read encodings, not coefficients |
| `ghostframe-lib/src/capture/gpu_pipeline/cdf53_forward_gate_tests.rs` | → `cdf53_encode_gate_tests.rs`, both variants |
| `.github/workflows/ci.yml`, `Justfile`, `AGENTS.md`, the spec | skip name, recipe, probe docs, decisions |

---

### Task 1: The encode variants of the shader

**Files:**
- Modify: `ghostframe-lib/src/capture/shaders/cdf53_encode.comp`
- Create: `ghostframe-lib/src/capture/shaders/cdf53_encode_sg.spv`, `cdf53_encode_wg.spv`
- Modify: `Justfile` (`shaders-cdf53`)
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/cdf53_shader_gen.rs`

- [ ] **Step 1: Take the verified source**

```bash
git show spike/cdf53-gpu:ghostframe-lib/src/capture/shaders/spike/cdf53_encode_pr2.comp \
    > ghostframe-lib/src/capture/shaders/cdf53_encode.comp
```

Its lifting block is empty. The forward part is identical to PR 1's, and the role function returns `gl_LocalInvocationIndex` for the forward-only variant.

- [ ] **Step 2: Compile every variant in the recipe**

Replace the `shaders-cdf53` recipe body in `Justfile` with:

```just
shaders-cdf53:
    cargo test -p ghostframe-lib --lib cdf53_shader_gen::regenerate -- --ignored
    glslc -O --target-env=vulkan1.1 -DCDF53_FORWARD_ONLY=1 \
        -o ghostframe-lib/src/capture/shaders/cdf53_forward.spv \
        ghostframe-lib/src/capture/shaders/cdf53_encode.comp
    glslc -O --target-env=vulkan1.1 -DCDF53_RLE_SUBGROUP=1 \
        -o ghostframe-lib/src/capture/shaders/cdf53_encode_sg.spv \
        ghostframe-lib/src/capture/shaders/cdf53_encode.comp
    glslc -O --target-env=vulkan1.1 \
        -o ghostframe-lib/src/capture/shaders/cdf53_encode_wg.spv \
        ghostframe-lib/src/capture/shaders/cdf53_encode.comp
```

Run:
```bash
just shaders-cdf53
for v in forward encode_sg encode_wg; do spirv-val --target-env vulkan1.1 ghostframe-lib/src/capture/shaders/cdf53_$v.spv && echo "$v VALID"; done
```
Expected: `forward VALID`, `encode_sg VALID`, `encode_wg VALID`. `git diff --stat ghostframe-lib/src/capture/shaders/cdf53_forward.spv` may show a change, since the source text changed. The forward's behaviour is pinned by the PR 1 gates, which Task 8 re-runs.

- [ ] **Step 3: Pin the constants the shader shares with Rust**

Append to `cdf53_shader_gen.rs`. These fail until Task 2 adds `cdf53_encode.rs`; that is the intended order.

```rust
/// The encode variants size their staging area and records from constants
/// the host also uses; the two must agree.
#[test]
fn shader_constants_match_the_host() {
    use super::cdf53_encode::{MAX_TILE_PAYLOAD, RECORD_WORDS};
    let src = std::fs::read_to_string(shader_path()).expect("read cdf53_encode.comp");
    assert!(
        src.contains(&format!("const uint STAGE_WORDS = {}u;", MAX_TILE_PAYLOAD / 4)),
        "STAGE_WORDS must be MAX_TILE_PAYLOAD / 4 = {}",
        MAX_TILE_PAYLOAD / 4
    );
    assert!(src.contains(&format!("const uint RECORD_WORDS = {RECORD_WORDS}u;")));
    assert_eq!(MAX_TILE_PAYLOAD % 4, 0);
}
```

- [ ] **Step 4: Commit** (the test above fails to compile until Task 2; commit them together at the end of Task 2 instead). Leave Task 1's changes uncommitted and go on.

---

### Task 2: Records, `Cdf53Encoded` and the decoder (CPU only, TDD)

**Files:**
- Create: `ghostframe-lib/src/capture/gpu_pipeline/cdf53_encode.rs`
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/mod.rs` (declare the module, re-export)
- Modify: `ghostframe-lib/src/transport/io_bridge/frame_backend.rs` (move `Cdf53Encoded`)

- [ ] **Step 1: The module with failing tests**

Create `ghostframe-lib/src/capture/gpu_pipeline/cdf53_encode.rs`:

```rust
//! CDF 5/3 tile encode on the GPU: what the host reads back, and how it is
//! checked before anything is sent.
//!
//! `cdf53_encode.comp` writes, per list slot, a record of `RECORD_WORDS`
//! u32 and the tile's payload, which is the concatenation of the passes
//! `cdf53::encode_passes_sparse` returns, in ascending pass order, at a
//! 4-byte aligned offset of a shared payload buffer. Record words:
//!
//! ```text
//! 0      byte offset of the payload        1  byte length
//! 2      present_passes (low 16) | status << 16 (0 ok, 1 no room)
//! 3..=9  per-pass byte lengths, u16 x 14, pass 0 in the low half of word 3
//! 10, 11 zero
//! ```
//!
//! The record is GPU-written memory: [`decode_record`] checks it fully and
//! never indexes past what the shader reports as used.

use crate::encoder::cdf53::{self, CDF53_PASS_COUNT, MAX_PASS_PAYLOAD};

/// One CDF 5/3 tile's passes, encoded: what `cdf53::encode_passes_sparse`
/// returns. `passes` is sorted by pass index; pass 0 is always present and
/// its payload starts with the 2-byte big-endian `present_passes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cdf53Encoded {
    pub present_passes: u16,
    pub passes: Vec<(u8, Vec<u8>)>,
}

impl Cdf53Encoded {
    /// The CPU encoder's result for a 32x32 BGRA tile: the oracle the GPU
    /// encode is held to, byte for byte.
    pub fn of_tile(tile_bgra: &[u8]) -> Self {
        let (present_passes, passes) = cdf53::encode_passes_sparse(&cdf53::forward(tile_bgra));
        Self {
            present_passes,
            passes,
        }
    }
}

/// u32 words per list slot in a records buffer.
pub(crate) const RECORD_WORDS: usize = 12;

/// Most payload bytes one tile can take: every pass at its ceiling.
pub(crate) const MAX_TILE_PAYLOAD: usize = CDF53_PASS_COUNT * MAX_PASS_PAYLOAD;

/// Why a record was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    /// The shader reported a status: 1 is "no room in the payload buffer".
    Status(u16),
    /// The offset is not a multiple of 4.
    Misaligned,
    /// The payload runs past what the shader reports as used.
    OutOfRange,
    /// Pass 0, which carries `present_passes`, is not marked present.
    Pass0Missing,
    /// A bit above the last pass is set in `present_passes`.
    UnknownPass,
    /// The per-pass lengths do not add up to the length.
    LengthsDisagree,
    /// A pass not marked present has bytes.
    AbsentPassHasBytes,
    /// A pass marked present has no bytes (every present pass has at least
    /// its three 2-byte length prefixes).
    PresentPassEmpty,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for RecordError {}

/// Check one slot's record against the payload buffer and cut the payload
/// into passes. `used` is the shader's byte counter; nothing at or past
/// `min(used, payload.len())` is read.
pub(crate) fn decode_record(
    rec: &[u32],
    payload: &[u8],
    used: usize,
) -> Result<Cdf53Encoded, RecordError> {
    assert_eq!(rec.len(), RECORD_WORDS, "one record is RECORD_WORDS words");
    let status = (rec[2] >> 16) as u16;
    if status != 0 {
        return Err(RecordError::Status(status));
    }
    let present = (rec[2] & 0xFFFF) as u16;
    let (off, len) = (rec[0] as usize, rec[1] as usize);
    if off % 4 != 0 {
        return Err(RecordError::Misaligned);
    }
    match off.checked_add(len) {
        Some(end) if end <= used.min(payload.len()) => {}
        _ => return Err(RecordError::OutOfRange),
    }
    if present & 1 == 0 {
        return Err(RecordError::Pass0Missing);
    }
    if present >> CDF53_PASS_COUNT != 0 {
        return Err(RecordError::UnknownPass);
    }
    let lens: Vec<usize> = (0..CDF53_PASS_COUNT)
        .map(|p| ((rec[3 + p / 2] >> (16 * (p % 2))) & 0xFFFF) as usize)
        .collect();
    if lens.iter().sum::<usize>() != len {
        return Err(RecordError::LengthsDisagree);
    }
    let mut passes = Vec::with_capacity(present.count_ones() as usize);
    let mut at = off;
    for (p, &l) in lens.iter().enumerate() {
        if present & (1 << p) == 0 {
            if l != 0 {
                return Err(RecordError::AbsentPassHasBytes);
            }
            continue;
        }
        if l == 0 {
            return Err(RecordError::PresentPassEmpty);
        }
        passes.push((p as u8, payload[at..at + l].to_vec()));
        at += l;
    }
    Ok(Cdf53Encoded {
        present_passes: present,
        passes,
    })
}

/// A 32x32 BGRA tile of pattern `kind` (0..=7); `seed` varies the noise.
/// Flat extremes, maximal high-pass energy (checkerboard, stripes), a lone
/// corner impulse (the mirror boundary), full-range noise (every plane, RLE
/// escapes) and a gradient (empty high passes, long zero runs).
pub(crate) fn test_pattern(kind: u32, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut t = vec![0u8; 32 * 32 * 4];
    for i in 0..1024usize {
        let (x, y) = (i % 32, i / 32);
        for c in 0..3usize {
            t[i * 4 + c] = match kind {
                0 => 0,
                1 => 255,
                2 => if (x + y + c) % 2 == 0 { 255 } else { 0 },
                3 => if x % 2 == 0 { 255 } else { 0 },
                4 => if y % 2 == 0 { 255 } else { 0 },
                5 => if x == 31 && y == 31 { 255 } else { 0 },
                6 => rnd() as u8,
                _ => ((x * 8 + y * 3 + c * 40) % 256) as u8,
            };
        }
        t[i * 4 + 3] = 255;
    }
    t
}

/// What the start-up self-test encodes: every pattern, and noise twice.
pub(crate) fn self_test_tiles() -> Vec<Vec<u8>> {
    let mut tiles: Vec<Vec<u8>> = (0..8u32).map(|k| test_pattern(k, 1)).collect();
    tiles.push(test_pattern(6, 2));
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record and payload buffer a correct shader would write for `enc`
    /// at byte offset `off`.
    fn pack(enc: &Cdf53Encoded, off: usize) -> (Vec<u32>, Vec<u8>) {
        let mut payload = vec![0u8; off];
        let mut rec = vec![0u32; RECORD_WORDS];
        let mut len = 0usize;
        for (p, bytes) in &enc.passes {
            let p = *p as usize;
            rec[3 + p / 2] |= (bytes.len() as u32) << (16 * (p % 2));
            payload.extend_from_slice(bytes);
            len += bytes.len();
        }
        rec[0] = off as u32;
        rec[1] = len as u32;
        rec[2] = enc.present_passes as u32;
        (rec, payload)
    }

    fn sample() -> Cdf53Encoded {
        Cdf53Encoded::of_tile(&test_pattern(7, 1))
    }

    #[test]
    fn a_correct_record_decodes_to_the_cpu_encoding() {
        for kind in 0..8 {
            let enc = Cdf53Encoded::of_tile(&test_pattern(kind, 3));
            let (rec, payload) = pack(&enc, 8);
            assert_eq!(decode_record(&rec, &payload, payload.len()), Ok(enc), "kind {kind}");
        }
    }

    #[test]
    fn a_status_is_refused() {
        let (mut rec, payload) = pack(&sample(), 0);
        rec[2] |= 1 << 16;
        assert_eq!(decode_record(&rec, &payload, payload.len()), Err(RecordError::Status(1)));
    }

    #[test]
    fn a_misaligned_offset_is_refused() {
        let (mut rec, mut payload) = pack(&sample(), 4);
        payload.insert(0, 0);
        rec[0] = 2;
        assert_eq!(decode_record(&rec, &payload, payload.len()), Err(RecordError::Misaligned));
    }

    #[test]
    fn bytes_past_the_used_counter_are_never_read() {
        let (rec, payload) = pack(&sample(), 4);
        assert_eq!(decode_record(&rec, &payload, payload.len() - 1), Err(RecordError::OutOfRange));
        let mut huge = rec.clone();
        huge[1] = u32::MAX;
        assert_eq!(decode_record(&huge, &payload, usize::MAX), Err(RecordError::OutOfRange));
    }

    #[test]
    fn pass_zero_must_be_present() {
        let (mut rec, payload) = pack(&sample(), 0);
        rec[2] &= !1;
        assert_eq!(decode_record(&rec, &payload, payload.len()), Err(RecordError::Pass0Missing));
    }

    #[test]
    fn a_bit_past_the_last_pass_is_refused() {
        let (mut rec, payload) = pack(&sample(), 0);
        rec[2] |= 1 << 15;
        assert_eq!(decode_record(&rec, &payload, payload.len()), Err(RecordError::UnknownPass));
    }

    #[test]
    fn lengths_that_do_not_add_up_are_refused() {
        let (mut rec, mut payload) = pack(&sample(), 0);
        rec[1] += 1;
        payload.push(0);
        assert_eq!(decode_record(&rec, &payload, payload.len()), Err(RecordError::LengthsDisagree));
    }

    #[test]
    fn an_absent_pass_with_bytes_is_refused() {
        // The gradient leaves high passes empty; give pass 13 a byte it does not have.
        let enc = sample();
        assert_eq!(enc.present_passes & (1 << 13), 0, "fixture: pass 13 absent");
        let (mut rec, mut payload) = pack(&enc, 0);
        rec[3 + 13 / 2] |= 1 << 16;
        rec[1] += 1;
        payload.push(0);
        assert_eq!(
            decode_record(&rec, &payload, payload.len()),
            Err(RecordError::AbsentPassHasBytes)
        );
    }

    #[test]
    fn a_present_pass_without_bytes_is_refused() {
        let enc = sample();
        assert_eq!(enc.present_passes & (1 << 13), 0, "fixture: pass 13 absent");
        let (mut rec, payload) = pack(&enc, 0);
        rec[2] |= 1 << 13;
        assert_eq!(
            decode_record(&rec, &payload, payload.len()),
            Err(RecordError::PresentPassEmpty)
        );
    }

    #[test]
    fn the_staging_area_holds_the_largest_tile() {
        assert_eq!(MAX_TILE_PAYLOAD, 14 * 776);
        for t in self_test_tiles() {
            let enc = Cdf53Encoded::of_tile(&t);
            let n: usize = enc.passes.iter().map(|(_, b)| b.len()).sum();
            assert!(n <= MAX_TILE_PAYLOAD);
        }
    }
}
```

In `mod.rs`, add with the non-test modules (next to `mod frame; mod setup;`):

```rust
mod cdf53_encode;
pub use cdf53_encode::{Cdf53Encoded, RecordError};
```

- [ ] **Step 2: Move `Cdf53Encoded`**

In `ghostframe-lib/src/transport/io_bridge/frame_backend.rs`, delete:

```rust
/// One CDF 5/3 tile's passes, encoded.
#[derive(Debug)]
pub struct Cdf53Encoded {
    pub present_passes: u16,
    pub passes: Vec<(u8, Vec<u8>)>,
}
```

and put this in its place:

```rust
pub use crate::capture::gpu_pipeline::Cdf53Encoded;
```

If `crate::capture::gpu_pipeline` is not reachable from there under that path, check how `frame_backend.rs` already imports `GpuFrameProcessor` (`grep -n "GpuFrameProcessor" frame_backend.rs | head -3`) and use the same path.

- [ ] **Step 3: Run the tests**

Run: `cargo test -p ghostframe-lib --lib cdf53_encode::tests cdf53_shader_gen`
Expected: all pass. That is 10 `cdf53_encode::tests`, plus `generated_lifting_matches_the_checked_in_shader` and `shader_constants_match_the_host`.

Then mutate `decode_record` and check a test notices:
- delete the `if l == 0 { return Err(RecordError::PresentPassEmpty); }` lines and re-run; `a_present_pass_without_bytes_is_refused` must FAIL;
- restore, and replace `used.min(payload.len())` with `payload.len()`; `bytes_past_the_used_counter_are_never_read` must FAIL;
- restore.

- [ ] **Step 4: Clippy and commit (Tasks 1 and 2)**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add ghostframe-lib/src/capture/shaders/cdf53_encode.comp ghostframe-lib/src/capture/shaders/cdf53_forward.spv \
        ghostframe-lib/src/capture/shaders/cdf53_encode_sg.spv ghostframe-lib/src/capture/shaders/cdf53_encode_wg.spv \
        Justfile ghostframe-lib/src/capture/gpu_pipeline/cdf53_shader_gen.rs \
        ghostframe-lib/src/capture/gpu_pipeline/cdf53_encode.rs ghostframe-lib/src/capture/gpu_pipeline/mod.rs \
        ghostframe-lib/src/transport/io_bridge/frame_backend.rs
git commit -m "feat(cdf53): GPU encode shader variants and the record decoder

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

Clippy may flag `RECORD_WORDS`, `MAX_TILE_PAYLOAD`, `decode_record`, `self_test_tiles` or `test_pattern` as dead code until Task 6 uses them. If so, add `#[cfg_attr(not(test), allow(dead_code))]` on each flagged item for now. Task 6 Step 6 removes these attributes.

---

### Task 3: Subgroup planning (CPU only, TDD)

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/cdf53_encode.rs`

- [ ] **Step 1: Failing tests**

Append to `cdf53_encode.rs`, above `#[cfg(test)] mod tests`:

```rust
/// Which encode pipeline a processor uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cdf53Encoder {
    /// RLE scans with subgroup shuffles in 32-lane clusters (`cdf53_encode_sg.spv`).
    Subgroup,
    /// RLE scans in workgroup memory, basic compute only (`cdf53_encode_wg.spv`).
    Workgroup,
}

/// What the device says about subgroups, as far as the encode cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubgroupCaps {
    /// `VkPhysicalDeviceSubgroupProperties::subgroupSize`.
    pub(crate) size: u32,
    /// The compute stage has the BASIC, BALLOT, SHUFFLE and SHUFFLE_RELATIVE operations.
    pub(crate) compute_ops: bool,
    /// `VK_EXT_subgroup_size_control`, when present with `subgroupSizeControl` on.
    pub(crate) size_control: Option<SizeControl>,
}

/// `VkPhysicalDeviceSubgroupSizeControlProperties`, the parts used here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SizeControl {
    pub(crate) min: u32,
    pub(crate) max: u32,
    /// `requiredSubgroupSizeStages` includes compute.
    pub(crate) compute: bool,
    pub(crate) max_compute_workgroup_subgroups: u32,
}

/// How the subgroup pipeline is created, when it can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubgroupPlan {
    /// The device's own subgroup size is a multiple of 32.
    Native,
    /// Pin the pipeline to 32 lanes with `VK_EXT_subgroup_size_control`.
    Pinned32,
}

/// Whether the subgroup variant can run here, and how. Its RLE scans need
/// every 32 roles of a channel in one aligned 32-lane cluster: a subgroup
/// size that is a multiple of 32, either the device's own or pinned.
pub(crate) fn plan_subgroups(c: &SubgroupCaps) -> Option<SubgroupPlan> {
    if !c.compute_ops {
        return None;
    }
    if c.size >= 32 && c.size % 32 == 0 {
        return Some(SubgroupPlan::Native);
    }
    match c.size_control {
        // 96 invocations in 32-lane subgroups is 3 of them.
        Some(s) if s.compute && s.min <= 32 && 32 <= s.max && s.max_compute_workgroup_subgroups >= 3 => {
            Some(SubgroupPlan::Pinned32)
        }
        _ => None,
    }
}
```

Append inside `mod tests`:

```rust
    fn caps(size: u32, ops: bool, ctl: Option<(u32, u32, bool, u32)>) -> SubgroupCaps {
        SubgroupCaps {
            size,
            compute_ops: ops,
            size_control: ctl.map(|(min, max, compute, n)| SizeControl {
                min,
                max,
                compute,
                max_compute_workgroup_subgroups: n,
            }),
        }
    }

    #[test]
    fn subgroup_plans_for_known_devices() {
        // RADV GCN (RX 480): wave64 fixed.
        assert_eq!(plan_subgroups(&caps(64, true, Some((64, 64, true, 16)))), Some(SubgroupPlan::Native));
        // NVIDIA: 32.
        assert_eq!(plan_subgroups(&caps(32, true, None)), Some(SubgroupPlan::Native));
        // Intel: reports 16, can be pinned anywhere in 8..=32.
        assert_eq!(plan_subgroups(&caps(16, true, Some((8, 32, true, 64)))), Some(SubgroupPlan::Pinned32));
        // Intel without the extension: no subgroup variant.
        assert_eq!(plan_subgroups(&caps(16, true, None)), None);
        // Missing operations.
        assert_eq!(plan_subgroups(&caps(64, false, None)), None);
        // Size control that cannot reach 32, or not for compute, or too few subgroups.
        assert_eq!(plan_subgroups(&caps(16, true, Some((8, 16, true, 64)))), None);
        assert_eq!(plan_subgroups(&caps(16, true, Some((8, 32, false, 64)))), None);
        assert_eq!(plan_subgroups(&caps(16, true, Some((8, 32, true, 2)))), None);
    }
```

- [ ] **Step 2: Run, then mutate**

Run: `cargo test -p ghostframe-lib --lib cdf53_encode::tests::subgroup_plans_for_known_devices`
Expected: PASS. Then change `c.size % 32 == 0` to `c.size % 16 == 0` and confirm the test FAILS. The `(16, true, None)` case would then return `Native`. Restore.

- [ ] **Step 3: Commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add ghostframe-lib/src/capture/gpu_pipeline/cdf53_encode.rs
git commit -m "feat(cdf53): choose the subgroup RLE variant from the device's subgroup caps

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 4: `GHOSTFRAME_CDF53_RLE`

**Files:**
- Modify: `ghostframe-lib/src/config.rs`

- [ ] **Step 1: Failing test**

In `config.rs`'s tests module, next to `the_h264_backend_choice_and_the_openh264_path`, add:

```rust
    #[test]
    fn the_cdf53_rle_choice() {
        let choice = |v: &str| TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_CDF53_RLE", v)])).cdf53_rle;
        assert_eq!(TransportConfig::from_lookup(lookup(&[])).cdf53_rle, Cdf53RleChoice::Auto);
        assert_eq!(TransportConfig::default().cdf53_rle, Cdf53RleChoice::Auto);
        assert_eq!(choice("auto"), Cdf53RleChoice::Auto);
        assert_eq!(choice(""), Cdf53RleChoice::Auto);
        assert_eq!(choice("wg"), Cdf53RleChoice::Workgroup);
        assert_eq!(choice(" wg "), Cdf53RleChoice::Workgroup);
        assert_eq!(choice("sg"), Cdf53RleChoice::Auto, "unknown: a warning and auto");
    }
```

Run `cargo test -p ghostframe-lib --lib config::tests::the_cdf53_rle_choice`. Expected: FAIL to compile (`Cdf53RleChoice` not found). Clippy shows it too.

- [ ] **Step 2: Implement**

After `impl H264BackendChoice { … }` in `config.rs`, add:

```rust
/// Which CDF 5/3 GPU encode variant the daemon may use
/// (`GHOSTFRAME_CDF53_RLE`). `auto` takes the subgroup variant where the
/// device supports it and it passes its start-up self-test, else the
/// workgroup-memory one; `wg` forces the latter (A/B runs, and exercising it
/// on hardware that would pick the subgroup one).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Cdf53RleChoice {
    #[default]
    Auto,
    Workgroup,
}

impl Cdf53RleChoice {
    /// An unset variable is `auto`; an unrecognised value is ignored with a
    /// warning.
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            None | Some("") | Some("auto") => Self::Auto,
            Some("wg") => Self::Workgroup,
            Some(other) => {
                tracing::warn!(
                    target: "ghostframe::config",
                    value = %other,
                    "GHOSTFRAME_CDF53_RLE is not auto|wg: using auto"
                );
                Self::Auto
            }
        }
    }
}
```

In `pub struct TransportConfig`, after the `h264_backend` field, add:

```rust
    /// The CDF 5/3 GPU encode variant, from `GHOSTFRAME_CDF53_RLE`
    /// (`auto` | `wg`). `auto` is the default.
    pub cdf53_rle: Cdf53RleChoice,
```

In `TransportConfig::from_lookup`, after the `h264_backend: …` initialiser, add:

```rust
            cdf53_rle: Cdf53RleChoice::parse(get("GHOSTFRAME_CDF53_RLE").as_deref()),
```

If `TransportConfig` has a hand-written `Default` impl (`grep -n "impl Default for TransportConfig" config.rs`), add `cdf53_rle: Cdf53RleChoice::Auto,` there. If it derives `Default`, nothing more is needed.

- [ ] **Step 3: Run and commit**

```bash
cargo test -p ghostframe-lib --lib config::
bash ci/check-env-reads.sh && echo ENV-OK
cargo clippy --workspace --all-targets -- -D warnings
git add ghostframe-lib/src/config.rs
git commit -m "feat(config): GHOSTFRAME_CDF53_RLE selects the CDF 5/3 GPU encode variant

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

Clippy may flag `cdf53_rle` as never read until Task 6. If it does, put `#[allow(dead_code)] // read by GpuFrameProcessor::with_cdf53_rle (next task)` on the field and remove it in Task 6.

---

### Task 5: Pipelines with a required subgroup size

**Files:**
- Modify: `ghostframe-lib/src/capture/pipeline_builder.rs`

- [ ] **Step 1: Implement**

Rename `pub unsafe fn build_compute_pipeline(` to `pub unsafe fn build_compute_pipeline_sized(`, and add a last parameter `required_subgroup_size: Option<u32>,`. Inside it, replace:

```rust
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(shader_module)
        .name(entry_name);
```

with:

```rust
    // `VK_EXT_subgroup_size_control`: pin the subgroup size when asked. The
    // device must have been created with that extension and its
    // `subgroupSizeControl` feature (setup.rs does when it plans `Pinned32`).
    let mut required = vk::PipelineShaderStageRequiredSubgroupSizeCreateInfo::default()
        .required_subgroup_size(required_subgroup_size.unwrap_or(0));
    let mut stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(shader_module)
        .name(entry_name);
    if required_subgroup_size.is_some() {
        stage = stage.push_next(&mut required);
    }
```

Then add the old name back as a wrapper, above it:

```rust
/// [`build_compute_pipeline_sized`] with the device's own subgroup size.
pub unsafe fn build_compute_pipeline(
    device: &ash::Device,
    bindings: &[BindingSpec],
    push_constant_size: u32,
    shader_module: vk::ShaderModule,
) -> Result<(vk::DescriptorSetLayout, vk::PipelineLayout, vk::Pipeline), vk::Result> {
    build_compute_pipeline_sized(device, bindings, push_constant_size, shader_module, None)
}
```

Keep the doc comment and `# Safety` section on the `_sized` function. Add one line to them: "`required_subgroup_size`: see `VkPipelineShaderStageRequiredSubgroupSizeCreateInfo`."

- [ ] **Step 2: Compile and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add ghostframe-lib/src/capture/pipeline_builder.rs
git commit -m "feat(pipeline_builder): compute pipelines with a required subgroup size

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

If clippy reports `build_compute_pipeline_sized` as unused, it is used only through the wrapper until Task 6. Add `#[allow(dead_code)]` only if a lint actually fires, and remove it in Task 6.

---

### Task 6: The processor: probe, pipelines, outputs, self-test

One compile unit. Steps 1–5 change setup, struct and construction; Step 6 builds and runs.

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/setup.rs`
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/mod.rs`
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/frame.rs` (fallback chunk only; Task 7 does the frame)
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: `Cdf53Out` (`mod.rs`)**

Add after `impl<T> HostBuffer<T> { … }`:

```rust
/// The outputs of one CDF 5/3 encode list: the packed payloads, one record
/// per list slot, and the payload bytes handed out (zeroed by the host
/// before each dispatch). Host-visible and persistently mapped; read only
/// after the fence of the submit that wrote them.
pub(super) struct Cdf53Out {
    pub(super) payload: HostBuffer<u8>,
    pub(super) records: HostBuffer<u32>,
    pub(super) used: HostBuffer<u32>,
    pub(super) slots: usize,
}

impl Cdf53Out {
    /// Room for `slots` tiles at their largest.
    pub(super) unsafe fn alloc(
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        slots: usize,
        label: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        use crate::capture::pipeline_builder::alloc_host_readback_buffer_mapped;
        use cdf53_encode::{MAX_TILE_PAYLOAD, RECORD_WORDS};
        let slots = slots.max(1);
        let (pb, pm, pp) = alloc_host_readback_buffer_mapped::<u8>(
            device,
            mem_props,
            (slots * MAX_TILE_PAYLOAD) as vk::DeviceSize,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &format!("{label} payload"),
        )?;
        let (rb, rm, rp) = alloc_host_readback_buffer_mapped::<u32>(
            device,
            mem_props,
            (slots * RECORD_WORDS * 4) as vk::DeviceSize,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &format!("{label} records"),
        )?;
        let (ub, um, up) = alloc_host_readback_buffer_mapped::<u32>(
            device,
            mem_props,
            4,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            &format!("{label} used"),
        )?;
        Ok(Self {
            payload: HostBuffer { buffer: pb, memory: pm, ptr: pp },
            records: HostBuffer { buffer: rb, memory: rm, ptr: rp },
            used: HostBuffer { buffer: ub, memory: um, ptr: up },
            slots,
        })
    }

    /// The payload buffer's size, the shader's `cap_bytes`.
    pub(super) fn cap_bytes(&self) -> u32 {
        (self.slots * cdf53_encode::MAX_TILE_PAYLOAD) as u32
    }

    /// # Safety
    /// Same contract as [`HostBuffer::destroy`].
    pub(super) unsafe fn destroy(&self, d: &ash::Device) {
        self.payload.destroy(d);
        self.records.destroy(d);
        self.used.destroy(d);
    }

    /// The first `n` slots, checked and cut into passes.
    ///
    /// # Safety
    /// The submit that wrote these buffers has completed (its fence waited),
    /// and none has been recorded since.
    pub(crate) unsafe fn decode(&self, n: usize) -> Vec<Result<Cdf53Encoded, RecordError>> {
        use cdf53_encode::{decode_record, MAX_TILE_PAYLOAD, RECORD_WORDS};
        let cap = self.slots * MAX_TILE_PAYLOAD;
        let used = (*self.used.ptr as usize).min(cap);
        let payload = std::slice::from_raw_parts(self.payload.ptr, cap);
        let records = std::slice::from_raw_parts(self.records.ptr, self.slots * RECORD_WORDS);
        (0..n.min(self.slots))
            .map(|s| decode_record(&records[s * RECORD_WORDS..(s + 1) * RECORD_WORDS], payload, used))
            .collect()
    }
}
```

`alloc_host_readback_buffer_mapped` lives in `crate::capture::pipeline_builder`. Check the module path with `grep -n "alloc_host_readback_buffer_mapped" setup.rs | head -2` and use the same import setup.rs uses.

- [ ] **Step 2: Struct fields (`mod.rs`)**

In `GpuFrameProcessor`, replace the escalation coefficient fields:

```rust
    pub(crate) cdf53_escalation_coefficients_buffer: vk::Buffer,
    pub(crate) cdf53_escalation_coefficients_memory: vk::DeviceMemory,
    /// CPU reads escalation forward coefficients via this pointer post-fence.
    pub cdf53_escalation_coefficients_ptr: *const i32,
```

and `cdf53_fallback_coefficients: HostBuffer<i32>,` with:

```rust
    /// CDF 5/3 encode outputs: the frame's dirty list (`max_tiles` slots),
    /// the escalation list and the fallback chunk (`MAX_ESCALATION_PER_FRAME`
    /// each).
    pub(crate) cdf53_dirty_out: Cdf53Out,
    pub(crate) cdf53_escalation_out: Cdf53Out,
    pub(super) cdf53_fallback_out: Cdf53Out,
    /// The encode pipelines: workgroup-memory RLE always, subgroup RLE when
    /// the device can run it (see `plan_subgroups`).
    pub(super) cdf53_encode_wg: ComputeStage,
    pub(super) cdf53_encode_sg: Option<ComputeStage>,
    /// The one the self-test chose.
    pub(super) cdf53_encoder: Cdf53Encoder,
```

Put `#[cfg(feature = "cdf53-diag")]` on each of the three dirty coefficient fields:
- `cdf53_coefficients_buffer`
- `cdf53_coefficients_memory`
- `cdf53_coefficients_ptr`

Their doc comment should start "Only under `cdf53-diag`: the forward's coefficients for the GPU-vs-CPU diff."

Add `Cdf53Encoder` to the `pub use cdf53_encode::{…}` line from Task 2: `pub use cdf53_encode::{Cdf53Encoded, Cdf53Encoder, RecordError};`.

In `Drop`:
- the escalation coefficient unmap/destroy/free lines become `self.cdf53_escalation_out.destroy(&self.device);`;
- `self.cdf53_fallback_coefficients.destroy(&self.device);` becomes `self.cdf53_fallback_out.destroy(&self.device);`;
- put the dirty coefficient buffer's unmap/destroy/free (find it with `grep -n "cdf53_coefficients_memory" mod.rs`) under `#[cfg(feature = "cdf53-diag")]`, as a block;
- next to `self.cdf53_forward.destroy(&self.device);`, add:

```rust
            self.cdf53_dirty_out.destroy(&self.device);
            self.cdf53_encode_wg.destroy(&self.device);
            if let Some(sg) = &self.cdf53_encode_sg {
                sg.destroy(&self.device);
            }
```

- [ ] **Step 3: Construction with the choice and the self-test (`mod.rs`)**

Replace `pub fn new(max_tiles: u32) -> …` with:

```rust
    /// Create a new `GpuFrameProcessor` capable of tracking up to `max_tiles`
    /// tiles, with the CDF 5/3 encode variant chosen automatically.
    pub fn new(max_tiles: u32) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_cdf53_rle(max_tiles, crate::config::Cdf53RleChoice::Auto)
    }

    /// As [`Self::new`], with the operator's `GHOSTFRAME_CDF53_RLE` choice.
    /// Fails if no CDF 5/3 encode variant reproduces the CPU encoder on this
    /// device (the start-up self-test).
    pub fn with_cdf53_rle(
        max_tiles: u32,
        cdf53_rle: crate::config::Cdf53RleChoice,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Test builds: GPU use is mutually exclusive with the VA-API encoder
        // tests, per call (never for the processor's lifetime: tests move
        // processors between threads and tasks). Reentrant per thread.
        #[cfg(test)]
        let _gpu = crate::gpu_test_lock::lock();
        let mut p = unsafe { Self::new_inner(max_tiles, cdf53_rle)? };
        p.select_cdf53_encoder()?;
        Ok(p)
    }

    /// Encode the self-test tiles with `encoder` and compare every byte with
    /// the CPU encoder.
    pub(crate) fn cdf53_self_test(&mut self, encoder: Cdf53Encoder) -> Result<(), String> {
        let tiles = cdf53_encode::self_test_tiles();
        let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
        let got = self
            .encode_cdf53_tiles_with(encoder, &refs)
            .map_err(|e| e.to_string())?;
        for (i, t) in tiles.iter().enumerate() {
            if got[i] != Cdf53Encoded::of_tile(t) {
                return Err(format!("self-test tile {i} differs from the CPU encoder"));
            }
        }
        Ok(())
    }

    /// Choose the CDF 5/3 encode variant: the subgroup one if it was built
    /// and passes the self-test, else the workgroup-memory one if it passes,
    /// else fail.
    fn select_cdf53_encoder(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut reason = "the device has no 32-lane subgroups, or GHOSTFRAME_CDF53_RLE=wg";
        if self.cdf53_encode_sg.is_some() {
            match self.cdf53_self_test(Cdf53Encoder::Subgroup) {
                Ok(()) => {
                    self.cdf53_encoder = Cdf53Encoder::Subgroup;
                    tracing::info!(encoder = "subgroup", "CDF 5/3 GPU encode");
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "CDF 5/3 subgroup encode failed its self-test; trying the workgroup one");
                    reason = "the subgroup variant failed its self-test";
                }
            }
        }
        self.cdf53_self_test(Cdf53Encoder::Workgroup).map_err(|e| {
            format!("CDF 5/3 GPU encode failed its self-test on this device ({e}); refusing to start")
        })?;
        self.cdf53_encoder = Cdf53Encoder::Workgroup;
        tracing::info!(encoder = "workgroup", reason, "CDF 5/3 GPU encode");
        Ok(())
    }
```

Bring `Cdf53Encoder` and `Cdf53Encoded` into scope at the top of `mod.rs` if the `pub use` above does not already do that for the rest of the module (it does: `pub use` imports into the module's namespace).

- [ ] **Step 4: `setup.rs`: probe, extension, pipelines, buffers**

`new_inner` takes the choice: change the signature to `pub(super) unsafe fn new_inner(max_tiles: u32, cdf53_rle: crate::config::Cdf53RleChoice) -> Result<Self, Box<dyn std::error::Error>>`.

(a) **Probe.** Right after the `tracing::info!(device = …, "selected Vulkan device");` line, add:

```rust
        // --- Subgroups, for the CDF 5/3 encode's RLE variant ---
        let has_size_ext = instance
            .enumerate_device_extension_properties(physical_device)?
            .iter()
            .any(|e| CStr::from_ptr(e.extension_name.as_ptr()) == ash::ext::subgroup_size_control::NAME);
        let mut sg_props = vk::PhysicalDeviceSubgroupProperties::default();
        let mut sz_props = vk::PhysicalDeviceSubgroupSizeControlProperties::default();
        {
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut sg_props);
            if has_size_ext {
                props2 = props2.push_next(&mut sz_props);
            }
            instance.get_physical_device_properties2(physical_device, &mut props2);
        }
        let mut sz_feat = vk::PhysicalDeviceSubgroupSizeControlFeatures::default();
        if has_size_ext {
            let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut sz_feat);
            instance.get_physical_device_features2(physical_device, &mut f2);
        }
        let needed_ops = vk::SubgroupFeatureFlags::BASIC
            | vk::SubgroupFeatureFlags::BALLOT
            | vk::SubgroupFeatureFlags::SHUFFLE
            | vk::SubgroupFeatureFlags::SHUFFLE_RELATIVE;
        let sg_caps = SubgroupCaps {
            size: sg_props.subgroup_size,
            compute_ops: sg_props.supported_stages.contains(vk::ShaderStageFlags::COMPUTE)
                && sg_props.supported_operations.contains(needed_ops),
            size_control: (has_size_ext && sz_feat.subgroup_size_control == vk::TRUE).then(|| {
                SizeControl {
                    min: sz_props.min_subgroup_size,
                    max: sz_props.max_subgroup_size,
                    compute: sz_props
                        .required_subgroup_size_stages
                        .contains(vk::ShaderStageFlags::COMPUTE),
                    max_compute_workgroup_subgroups: sz_props.max_compute_workgroup_subgroups,
                }
            }),
        };
        let sg_plan = match cdf53_rle {
            crate::config::Cdf53RleChoice::Workgroup => None,
            crate::config::Cdf53RleChoice::Auto => plan_subgroups(&sg_caps),
        };
        tracing::debug!(?sg_caps, ?sg_plan, "CDF 5/3 encode: subgroup plan");
```

Import at the top of `setup.rs`: `use super::cdf53_encode::{plan_subgroups, Cdf53Encoder, SizeControl, SubgroupCaps, SubgroupPlan};`. If ash 0.38 names any of these types with an `EXT` suffix only (`grep -n "pub struct PhysicalDeviceSubgroupSizeControl" ~/.cargo/registry/src/*/ash-0.38*/src/vk/definitions.rs | head`), use the name it defines.

(b) **Extension and feature.** Where the device is created, replace:

```rust
        let ext_names_raw: Vec<*const i8> =
            required_device_exts.iter().map(|e| e.as_ptr()).collect();

        let device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_ci)
            .enabled_extension_names(&ext_names_raw);
```

with this. Keep the raw pointer type exactly as the original line spells it:

```rust
        let pin_subgroups = sg_plan == Some(SubgroupPlan::Pinned32);
        let mut device_exts: Vec<&CStr> = required_device_exts.to_vec();
        if pin_subgroups {
            device_exts.push(ash::ext::subgroup_size_control::NAME);
        }
        let ext_names_raw: Vec<*const i8> = device_exts.iter().map(|e| e.as_ptr()).collect();

        let mut enable_size_control =
            vk::PhysicalDeviceSubgroupSizeControlFeatures::default().subgroup_size_control(true);
        let mut device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_ci)
            .enabled_extension_names(&ext_names_raw);
        if pin_subgroups {
            device_ci = device_ci.push_next(&mut enable_size_control);
        }
```

(c) **Binding table and pool.** Add next to `CDF53_FORWARD_BINDINGS`:

```rust
const CDF53_ENCODE_BINDINGS: [BindingSpec; 5] = [
    BindingSpec::storage_image(0),
    BindingSpec::storage_buffer(1),
    BindingSpec::storage_buffer(2),
    BindingSpec::storage_buffer(3),
    BindingSpec::storage_buffer(4),
];
```

In `STAGE_POOL_USES`, the array length goes from 12 to 13. Replace `(&CDF53_FORWARD_BINDINGS, 2),` with:

```rust
    (&CDF53_ENCODE_BINDINGS, 2),
    // Only dispatched under `cdf53-diag`; always counted, so pool sizes do
    // not depend on features.
    (&CDF53_FORWARD_BINDINGS, 1),
```

Update the doc line above the array: "The cdf53 encode stage runs twice (refinement and escalation)".

In `pool_tests`, the encode stage adds (1 image + 4 buffers) × 2 and 2 sets, and the forward drops from 2 uses to 1. So:

```rust
        assert_eq!(POOL_NEED, (8, 39, 14));
        assert_eq!(
            (POOL_STORAGE_IMAGES, POOL_STORAGE_BUFFERS, POOL_MAX_SETS),
            (9, 39, 17)
        );
```

(d) **Pipelines.** After the forward pipeline block, add:

```rust
        // --- Cdf53 encode (Stage 4c): forward + bit-planes + RLE + packing ---
        // Bindings: 0 image, 1 tile list, 2 payload, 3 records, 4 used.
        // Push constants: 3 × u32 = 12 bytes (cols, rows, cap_bytes).
        let make_encode = |spv: &[u8], subgroup: Option<u32>| -> Result<ComputeStage, Box<dyn std::error::Error>> {
            let words = ash::util::read_spv(&mut std::io::Cursor::new(spv))?;
            let module = device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)?;
            match pipeline_builder::build_compute_pipeline_sized(&device, &CDF53_ENCODE_BINDINGS, 12, module, subgroup) {
                Ok((dsl, layout, pipeline)) => Ok(ComputeStage { module, dsl, layout, pipeline }),
                Err(e) => {
                    device.destroy_shader_module(module, None);
                    Err(e.into())
                }
            }
        };
        let cdf53_encode_wg = make_encode(include_bytes!("../shaders/cdf53_encode_wg.spv"), None)?;
        let cdf53_encode_sg = match sg_plan {
            None => None,
            Some(plan) => {
                let size = (plan == SubgroupPlan::Pinned32).then_some(32);
                match make_encode(include_bytes!("../shaders/cdf53_encode_sg.spv"), size) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!(error = %e, "CDF 5/3 subgroup encode pipeline not created; using the workgroup one");
                        None
                    }
                }
            }
        };
```

If the closure cannot borrow `device` across the `unsafe` calls (it should, since `new_inner` is an `unsafe fn` and the calls are inside it), make it a small `unsafe fn make_encode_stage(device: &ash::Device, spv: &[u8], subgroup: Option<u32>) -> Result<ComputeStage, Box<dyn std::error::Error>>` at module level with the same body.

(e) **Buffers.** Delete the escalation coefficients allocation (`cdf53_escalation_coefficients_size` and the `alloc_host_readback_buffer_mapped` call after it) and the fallback coefficients allocation (`fb_coeff_buffer`, …). Keep `cdf53_escalation_list_size`: the fallback list uses it. Put the dirty coefficient allocation (`cdf53_coefficients_size` … `let cdf53_coefficients_ptr = …;`) under `#[cfg(feature = "cdf53-diag")]`, as a block that binds the three names. Add:

```rust
        // --- Cdf53 encode outputs (Stage 4c/4d and the fallback) ---
        let cdf53_dirty_out = Cdf53Out::alloc(&device, &mem_props, max_tiles as usize, "cdf53 dirty")?;
        let cdf53_escalation_out = Cdf53Out::alloc(
            &device,
            &mem_props,
            crate::capture::gpu_pipeline::MAX_ESCALATION_PER_FRAME,
            "cdf53 escalation",
        )?;
        let cdf53_fallback_out = Cdf53Out::alloc(
            &device,
            &mem_props,
            crate::capture::gpu_pipeline::MAX_ESCALATION_PER_FRAME,
            "cdf53 fallback",
        )?;
```

In the `Ok(Self { … })` initialiser:
- remove the three `cdf53_escalation_coefficients_*` fields and `cdf53_fallback_coefficients: HostBuffer { … }`;
- put `#[cfg(feature = "cdf53-diag")]` on each of the three dirty coefficient fields;
- add:

```rust
            cdf53_dirty_out,
            cdf53_escalation_out,
            cdf53_fallback_out,
            cdf53_encode_wg,
            cdf53_encode_sg,
            // Replaced by `select_cdf53_encoder` before `new` returns.
            cdf53_encoder: Cdf53Encoder::Workgroup,
```

- [ ] **Step 5: The fallback chunk encodes (`frame.rs`)**

Add the stage recorder next to `run_cdf53_forward_stage`, and put `#[cfg(feature = "cdf53-diag")]` on `run_cdf53_forward_stage` itself:

```rust
    /// Stage 4c — CDF 5/3 encode: forward, bit-planes, RLE and packing, one
    /// dispatch. Zeroes `out.used`, dispatches `encoder`'s pipeline over
    /// `list_buffer`, and makes the three outputs visible to the host.
    ///
    /// Caller must:
    ///   - Ensure `current_view` is in `GENERAL` layout.
    ///   - For `StageDispatch::Indirect`, ensure the args buffer is barriered
    ///     for `INDIRECT_COMMAND_READ`.
    ///   - Hold the returned guard alive until after `wait_for_fences`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn run_cdf53_encode_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        encoder: Cdf53Encoder,
        current_view: vk::ImageView,
        cols: u32,
        rows: u32,
        list_buffer: vk::Buffer,
        out: &Cdf53Out,
        dispatch: StageDispatch<'_>,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        let stage = match encoder {
            Cdf53Encoder::Workgroup => &self.cdf53_encode_wg,
            Cdf53Encoder::Subgroup => self
                .cdf53_encode_sg
                .as_ref()
                .ok_or("the subgroup CDF 5/3 encode pipeline was not created")?,
        };
        self.device.cmd_fill_buffer(cmd, out.used.buffer, 0, 4, 0);
        let zeroed = buf_barrier(
            out.used.buffer,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            vk::WHOLE_SIZE,
        );
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&zeroed),
            &[],
        );
        let guard = self.record_stage(
            cmd,
            stage,
            &[
                StageBinding::Image(current_view),
                StageBinding::Buffer(list_buffer),
                StageBinding::Buffer(out.payload.buffer),
                StageBinding::Buffer(out.records.buffer),
                StageBinding::Buffer(out.used.buffer),
            ],
            &[cols, rows, out.cap_bytes()],
            dispatch,
        )?;
        let to_host = [out.payload.buffer, out.records.buffer, out.used.buffer].map(|b| {
            buf_barrier(b, vk::AccessFlags::SHADER_WRITE, vk::AccessFlags::HOST_READ, vk::WHOLE_SIZE)
        });
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &to_host,
            &[],
        );
        Ok(guard)
    }
```

Rename the fallback functions and switch them to encoding:
- `pub fn forward_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<i16>, …>` becomes `pub fn encode_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<Cdf53Encoded>, Box<dyn std::error::Error>>`. Its body calls `self.encode_cdf53_tiles_with(self.cdf53_encoder, tiles)`.
- Add `pub(crate) fn encode_cdf53_tiles_with(&mut self, encoder: Cdf53Encoder, tiles: &[&[u8]]) -> Result<Vec<Cdf53Encoded>, Box<dyn std::error::Error>>`. It holds the old body (size check, chunk loop), with `Vec<Cdf53Encoded>` as the accumulator and `self.encode_cdf53_chunk(encoder, chunk, &mut out)` in the loop.
- Rename `forward_cdf53_chunk` to `encode_cdf53_chunk(&mut self, encoder: Cdf53Encoder, chunk: &[&[u8]], out: &mut Vec<Cdf53Encoded>)`.

Inside the chunk, replace the `run_cdf53_forward_stage(…)` call and the host barrier after it with:

```rust
            let guard = self.run_cdf53_encode_stage(
                cmd,
                encoder,
                current.view,
                cols,
                rows,
                self.cdf53_fallback_list.buffer,
                &self.cdf53_fallback_out,
                StageDispatch::Direct(n as u32, 1, 1),
            )?;
```

After `self.submit_frame_cmd_and_wait(cmd)?;`, replace the coefficient read (`let coeffs = …; out.extend(…);`) with:

```rust
            for (i, r) in self.cdf53_fallback_out.decode(n).into_iter().enumerate() {
                out.push(r.map_err(|e| format!("cdf53 fallback tile {i}: {e}"))?);
            }
```

Update the doc comments: "Returns one `Cdf53Encoded` per tile, in order, byte-identical to `Cdf53Encoded::of_tile`."

- [ ] **Step 6: The daemon passes the operator's choice (`io_bridge.rs`)**

In `IoBridge::new`, replace `GpuFrameProcessor::new(crate::capture::gpu_pipeline::MAX_FRAME_TILES,)` with:

```rust
GpuFrameProcessor::with_cdf53_rle(
            crate::capture::gpu_pipeline::MAX_FRAME_TILES,
            lib_config.transport.cdf53_rle,
        )
```

Remove any `#[allow(dead_code)]` / `#[cfg_attr(not(test), allow(dead_code))]` added in Tasks 2, 4 and 5 that the code now uses. Re-run clippy to confirm.

Task 7 rewires the frame and its consumers. Until then the tree does not compile: `record_shared_stages` and `frame_backend` still read the coefficient buffers this task removed. Go straight on to Task 7 and commit them together.

---

### Task 7: The frame encodes; the consumers read records

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/frame.rs`
- Modify: `ghostframe-lib/src/transport/io_bridge/frame_backend.rs`, `frame_worker.rs`, `emit.rs`, `frame.rs`, `unchanged_oracle_tests.rs`

- [ ] **Step 1: `record_shared_stages`**

Replace the Stage 4c block that PR 1 wrote (from `let cdf53_forward_ds_guard = self.run_cdf53_forward_stage(` through its HOST_READ barrier) with:

```rust
        let cdf53_encode_ds_guard = self.run_cdf53_encode_stage(
            cmd,
            self.cdf53_encoder,
            current_view,
            cols,
            rows,
            self.cdf53_compact_list_buffer,
            &self.cdf53_dirty_out,
            StageDispatch::Indirect(self.cdf53_dispatch_args.buffer),
        )?;

        // `cdf53-diag`: the forward's coefficients too, for the GPU-vs-CPU diff.
        #[cfg(feature = "cdf53-diag")]
        let cdf53_diag_forward_ds_guard = {
            let guard = self.run_cdf53_forward_stage(
                cmd,
                current_view,
                cols,
                rows,
                self.cdf53_compact_list_buffer,
                self.cdf53_coefficients_buffer,
                StageDispatch::Indirect(self.cdf53_dispatch_args.buffer),
            )?;
            let b = buf_barrier(
                self.cdf53_coefficients_buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            );
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                std::slice::from_ref(&b),
                &[],
            );
            guard
        };
```

In Stage 4d, replace the `run_cdf53_forward_stage(…)` call and its HOST_READ barrier with:

```rust
            let guard = self.run_cdf53_encode_stage(
                cmd,
                self.cdf53_encoder,
                current_view,
                cols,
                rows,
                self.cdf53_escalation_list.buffer,
                &self.cdf53_escalation_out,
                StageDispatch::Direct(escalation_count, 1, 1),
            )?;
```

- [ ] **Step 2: `SharedStageGuards`**

- Rename the field `cdf53_forward_ds_guard` to `cdf53_encode_ds_guard` everywhere: the struct, the `Ok(SharedStageGuards { … })`, and both `release_*` patterns and drops.
- Add a field after it: `#[cfg(feature = "cdf53-diag")] cdf53_diag_forward_ds_guard: ScopedDescriptorSets<'a>,`.
- Add the same cfg-gated name to the initialiser and to both destructuring patterns.
- Drop it right after `cdf53_encode_ds_guard`, in both release functions: `#[cfg(feature = "cdf53-diag")] drop(cdf53_diag_forward_ds_guard);`.
- In the step comment near line 1003, write `cdf53_encode_ds_guard, cdf53_escalation_ds_guard,`.

- [ ] **Step 3: The trait and its callers**

`frame_backend.rs`:
- In `trait FrameBackend`, replace `fn forward_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<i16>, String>;` with `fn encode_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<Cdf53Encoded>, String>;`, and its doc's "CDF 5/3 forward transform of the given 32x32 BGRA tiles … `CDF53_TOTAL_COEFFS` coefficients per tile, in order." with "CDF 5/3 encode of the given 32x32 BGRA tiles: one `Cdf53Encoded` per tile, in order."
- In `impl FrameBackend for GpuBackend`, rename the method the same way; it calls `self.processor.encode_cdf53_tiles(tiles)`. The `debug_assert!` message becomes "encode_cdf53_tiles during a frame in flight".

`frame_worker.rs`:
- `ToWorker::Forward { tiles, reply: std_mpsc::Sender<Result<Vec<i16>, String>> }` becomes `ToWorker::Encode { tiles: Vec<Vec<u8>>, reply: std_mpsc::Sender<Result<Vec<Cdf53Encoded>, String>> }`.
- `FramePipeline::forward_cdf53_tiles` becomes `encode_cdf53_tiles`, returning `Result<Vec<Cdf53Encoded>, String>`. Rename its `forward_requests` counter use only if the counter is private to this file (`grep -rn forward_requests ghostframe-lib/src`); if it is read elsewhere, keep its name.
- The worker arm becomes `ToWorker::Encode { tiles, reply } => { … locked(|| backend.encode_cdf53_tiles(&refs)) … }`.
- Import `Cdf53Encoded` from `super::frame_backend`.

`unchanged_oracle_tests.rs`: the impl becomes

```rust
    fn encode_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<Cdf53Encoded>, String> {
        Ok(tiles.iter().map(|t| Cdf53Encoded::of_tile(t)).collect())
    }
```

and its `encode` helper (around line 129) becomes `Cdf53Encoded::of_tile(&tile)` if it builds the same thing by hand. Read it first.

`io_bridge/tests.rs` (the scripted backend and a wrapper):
- The scripted backend's `fn forward_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<i16>, String>` (around line 1145) becomes:

```rust
    fn encode_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<Cdf53Encoded>, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("encode_cdf53_tiles {}", tiles.len()));
        if self.fail_forward {
            return Err("injected".into());
        }
        // Test scaffolding: the CPU encoder stands in for the GPU one.
        Ok(tiles.iter().map(|t| Cdf53Encoded::of_tile(t)).collect())
    }
```

- The wrapper around line 1562 forwards `encode_cdf53_tiles`.
- Every log string and assertion `"forward_cdf53_tiles …"` / `starts_with("forward_cdf53_tiles")` becomes `"encode_cdf53_tiles …"`.
- The `fail_forward` doc line says "`encode_cdf53_tiles` fails, …". Keep the field name `fail_forward`: renaming it is churn.
- Import `Cdf53Encoded` in that module (`use super::frame_backend::Cdf53Encoded;`, or whatever path the module already uses for `frame_backend` items).
- `gpu_backend_pre_encodes_fallback_tiles_exactly_as_the_cpu_transform` keeps its body. It compares `out.cdf53_fallback` with the CPU encoding already; its doc says "CPU transform", which stays true.

`emit.rs`: in the fallback block, replace everything from `let n = crate::encoder::cdf53::CDF53_TOTAL_COEFFS;` through the end of the `match coeffs { … }` with:

```rust
            // A round trip to the frame worker, which owns the GPU. The loop
            // waits for it (see `FramePipeline::encode_cdf53_tiles`).
            let encoded_by_gpu = self
                .frame_pipeline
                .as_ref()
                .map(|p| p.encode_cdf53_tiles(&refs));
            let encodings: Option<Vec<super::frame_backend::Cdf53Encoded>> = match encoded_by_gpu {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => {
                    tracing::error!(target: LOG_TARGET, error = %e, tiles = missing.len(),
                        "cdf53 fallback encode failed; tiles not sent this frame");
                    // Report the whole GPU baseline as stale so the next frame
                    // marks these tiles dirty again and they are retried.
                    self.backend_apply(super::frame_backend::Control::InvalidateBaseline(1));
                    None
                }
                #[cfg(test)]
                None => Some(
                    refs.iter()
                        .map(|t| super::frame_backend::Cdf53Encoded::of_tile(t))
                        .collect(),
                ),
                #[cfg(not(test))]
                None => {
                    tracing::error!(target: LOG_TARGET, tiles = missing.len(),
                        "cdf53 fallback needs the frame backend; tiles not sent this frame");
                    None
                }
            };
            match encodings {
                Some(v) => {
                    for (&xy, e) in missing.iter().zip(v) {
                        encoded.insert(xy, e);
                    }
                }
                None => failed = missing,
            }
```

Before replacing, read the current block. If its wording differs from what this plan quotes (it was copied from master at 8626c5c1), keep its log messages and behaviour, and change only coefficients → encodings.

- [ ] **Step 4: `frame_backend::finish` reads records**

Timings: in `TileTimings`, replace `pub coef_read_ns: u64, pub coef_encode_ns: u64,` with:

```rust
    /// Checking the GPU's CDF 5/3 records and copying the payloads out.
    pub payload_copy_ns: u64,
```

In `io_bridge/frame.rs`:
- `let (coef_read_ns, coef_encode_ns) = (timings.coef_read_ns, timings.coef_encode_ns);` becomes `let payload_copy_ns = timings.payload_copy_ns;`;
- the trace field `coef_read_us = coef_read_ns / 1000, coef_encode_us = coef_encode_ns / 1000,` becomes `cdf53_payload_us = payload_copy_ns / 1000,`.

Pixels for re-encoding: the fallback block further down works out `source_pixels` from `pixels` (`Ok(PixelSource::Frame) => &frame.pixels`, …, `Err(_) => &[]`). Move that `let source_pixels: &[u8] = match &pixels { … };` up to just before the dirty list, so the dirty, escalation and fallback steps all share it. Its last use is still before `TileOut { pixels, … }` moves `pixels`. Leave the fallback block using the hoisted binding.

Dirty list: replace the loop body that narrows i32 → i16 and calls `encode_passes_sparse` with this code. Keep the list read, the clamp and the `flat_tile_idx >= tiles` guard as they are. A refused tile is still a dirty tile, so it goes into `in_dirty_list` and is re-encoded after the escalation step:

```rust
        // Tiles whose GPU record was refused: re-encoded from the pixels below.
        let mut refused_dirty: Vec<u32> = Vec::new();
        if cdf53_count > 0 {
            let list = unsafe {
                std::slice::from_raw_parts(self.processor.cdf53_compact_list_ptr, cdf53_count)
            };
            lap.lap_ns();
            // SAFETY: the frame's fence has been waited (we are in `finish`).
            let decoded = unsafe { self.processor.cdf53_dirty_out.decode(cdf53_count) };
            for (slot, (&flat_tile_idx, encoded)) in list.iter().zip(decoded).enumerate() {
                // Also GPU-written: an index off the grid would wrap in the u8
                // casts below and address the wrong tile.
                if flat_tile_idx as usize >= tiles {
                    continue;
                }
                in_dirty_list.insert(flat_tile_idx);
                let encoded = match encoded {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(target: LOG_TARGET, slot, tile = flat_tile_idx, error = %e,
                            "cdf53 GPU record refused; encoding the tile again");
                        refused_dirty.push(flat_tile_idx);
                        continue;
                    }
                };
                cdf53_dirty.push(Cdf53Tile {
                    tile_x: (flat_tile_idx % cols) as u8,
                    tile_y: (flat_tile_idx / cols) as u8,
                    encoded,
                    #[cfg(feature = "cdf53-diag")]
                    coeffs_i16: unsafe {
                        std::slice::from_raw_parts(
                            self.processor
                                .cdf53_coefficients_ptr
                                .add(slot * crate::encoder::cdf53::CDF53_TOTAL_COEFFS),
                            crate::encoder::cdf53::CDF53_TOTAL_COEFFS,
                        )
                    }
                    .iter()
                    .map(|&v| v as i16)
                    .collect(),
                });
            }
            timings.payload_copy_ns += lap.lap_ns();
        }
```

Check the logging target constant used in this file (`grep -n "LOG_TARGET" frame_backend.rs | head -2`). If there is none, use `target: "ghostframe::io_bridge"`. `tracing::warn!` with field `slot` uses the variable, so no unused-variable warning arises without `cdf53-diag`.

Escalation: replace its loop so the coefficient reading becomes:

```rust
        let mut cdf53_escalation = Vec::with_capacity(req.escalation.len());
        // Escalation slots whose GPU record was refused: re-encoded below.
        let mut refused_esc: Vec<usize> = Vec::new();
        if !req.escalation.is_empty() {
            lap.lap_ns();
            // SAFETY: as above.
            let decoded = unsafe { self.processor.cdf53_escalation_out.decode(req.escalation.len()) };
            for (k, (s, encoded)) in req.escalation.iter().zip(decoded).enumerate() {
                if !s.stranded && (s.tile_is_cdf53 || in_dirty_list.contains(&s.tile_idx)) {
                    cdf53_escalation.push(None);
                    continue;
                }
                match encoded {
                    Ok(e) => cdf53_escalation.push(Some(e)),
                    Err(e) => {
                        tracing::warn!(target: LOG_TARGET, tile = s.tile_idx, error = %e,
                            "cdf53 GPU escalation record refused; encoding the tile again");
                        refused_esc.push(k);
                        cdf53_escalation.push(None);
                    }
                }
            }
            timings.payload_copy_ns += lap.lap_ns();
        }
```

Keep the comment block above it, about skipping tiles already CDF 5/3.

Right after it, re-encode the refused tiles as new tiles: one fallback-encode round trip, back into the lists they came from. A tile of a frame whose pixels could not be read (`source_pixels` empty) cannot be re-encoded and goes straight to the last resort.

```rust
        // A refused record (owner decision 2026-11-07): the tile is a new tile.
        // Encode it again from this frame's pixels and emit it where it came
        // from. Only if that fails too is the whole baseline invalidated, so the
        // next frame repaints it (it is already in the SAD reference).
        if !refused_dirty.is_empty() || !refused_esc.is_empty() {
            let flat: Vec<u32> = refused_dirty
                .iter()
                .copied()
                .chain(refused_esc.iter().map(|&k| req.escalation[k].tile_idx))
                .collect();
            let reencoded = if source_pixels.is_empty() {
                Err("no pixels for this frame".to_string())
            } else {
                let grid = crate::tile::TileGrid::new(frame.width, frame.height);
                let tiles_px: Vec<Vec<u8>> = flat
                    .iter()
                    .map(|&t| grid.extract_tile(source_pixels, frame.stride, t % cols, t / cols))
                    .collect();
                let refs: Vec<&[u8]> = tiles_px.iter().map(|t| t.as_slice()).collect();
                self.processor
                    .encode_cdf53_tiles(&refs)
                    .map(|enc| (tiles_px, enc))
                    .map_err(|e| e.to_string())
            };
            match reencoded {
                Ok((_tiles_px, enc)) => {
                    let mut enc = enc.into_iter();
                    for (i, &t) in refused_dirty.iter().enumerate() {
                        cdf53_dirty.push(Cdf53Tile {
                            tile_x: (t % cols) as u8,
                            tile_y: (t / cols) as u8,
                            encoded: enc.next().expect("one encoding per tile"),
                            // The diff compares against the CPU transform of
                            // the same pixels: here it is that transform.
                            #[cfg(feature = "cdf53-diag")]
                            coeffs_i16: crate::encoder::cdf53::forward(&_tiles_px[i]),
                        });
                        #[cfg(not(feature = "cdf53-diag"))]
                        let _ = i;
                    }
                    for &k in &refused_esc {
                        cdf53_escalation[k] = Some(enc.next().expect("one encoding per tile"));
                    }
                }
                Err(e) => {
                    tracing::warn!(target: LOG_TARGET, error = %e,
                        tiles = refused_dirty.len() + refused_esc.len(),
                        "cdf53 re-encode of refused records failed; baseline invalidated");
                    self.processor.invalidate_baseline(1);
                }
            }
        }
```

The `fallback_not_in_compact_list(…, &in_dirty_list)` call that follows already sees refused dirty tiles as dirty, because they were inserted above. So a tile is never both re-encoded here and pre-encoded as a fallback.

Fallback: replace `match self.processor.forward_cdf53_tiles(&refs) { Ok(coeffs) => { … encode_passes_sparse … } Err(e) => {…} }` with:

```rust
            match self.processor.encode_cdf53_tiles(&refs) {
                Ok(encodings) => {
                    cdf53_fallback.extend(fallback.iter().copied().zip(encodings));
                }
                Err(e) => {
                    tracing::warn!(target: LOG_TARGET, error = %e, tiles = fallback.len(),
                        "cdf53 fallback encode failed in the frame's finish; the commit retries");
                }
            }
            timings.payload_copy_ns += lap.lap_ns();
```

Keep the existing `Err` message's wording if it differs; change only "forward" to "encode".

Add two test-only hooks, used by the refused-record test in Task 8. In `frame_backend.rs`, in `impl GpuBackend`:

```rust
    /// Test-only: the processor, to tamper with its outputs between
    /// `analyse` and `finish_tile`.
    #[cfg(test)]
    pub(crate) fn processor_mut(&mut self) -> &mut GpuFrameProcessor {
        &mut self.processor
    }
```

In `gpu_pipeline/mod.rs`, in `impl GpuFrameProcessor`:

```rust
    /// Test-only: make dirty-list record `slot` report status 1, as the shader
    /// does when the payload buffer has no room.
    #[cfg(test)]
    pub(crate) fn refuse_cdf53_dirty_record(&mut self, slot: usize) {
        assert!(slot < self.cdf53_dirty_out.slots);
        // SAFETY: in bounds (checked); the buffer is host-coherent and mapped.
        unsafe {
            *self
                .cdf53_dirty_out
                .records
                .ptr
                .add(slot * cdf53_encode::RECORD_WORDS + 2) |= 1 << 16;
        }
    }
```

- [ ] **Step 5: Build**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings
git grep -n "forward_cdf53_tiles\|cdf53_escalation_coefficients\|cdf53_fallback_coefficients\|coef_read_ns\|coef_encode_ns\|ToWorker::Forward" -- ':!docs/superpowers'
```
Expected: clippy clean twice. The grep finds only the test files Task 8 rewrites (`tests.rs`, `damage_tests.rs`, `fallback_tests.rs`, `readback_bench.rs`, `cdf53_forward_gate_tests.rs`, `cdf53_spike.rs` if present). If clippy fails because of those test files, that is expected: carry straight on into Task 8 and commit at its end. Do not paper over test files with `#[cfg(any())]`.

---

### Task 8: Tests read encodings

**Files:**
- Rename: `cdf53_forward_gate_tests.rs` → `cdf53_encode_gate_tests.rs` (both in `ghostframe-lib/src/capture/gpu_pipeline/`)
- Modify: `tests.rs`, `damage_tests.rs`, `fallback_tests.rs`, `readback_bench.rs`, `mod.rs`, `.github/workflows/ci.yml`

- [ ] **Step 1: The encode gate**

```bash
git mv ghostframe-lib/src/capture/gpu_pipeline/cdf53_forward_gate_tests.rs \
       ghostframe-lib/src/capture/gpu_pipeline/cdf53_encode_gate_tests.rs
```

Replace its contents with:

```rust
//! The server GPU's CDF 5/3 encode against the CPU encoder,
//! `Cdf53Encoded::of_tile` (`cdf53::forward` + `encode_passes_sparse`),
//! byte for byte, through each entry point: the fallback chunk (both
//! variants), the frame's compact list and the escalation list. Needs a
//! Vulkan GPU; CI skips this module by name.

use super::cdf53_encode::test_pattern;
use super::*;

const NEEDS_GPU: &str =
    "GpuFrameProcessor::new failed: this test needs a Vulkan GPU and must not skip";

fn noise_frame(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..w * h)
        .flat_map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            [s as u8, (s >> 8) as u8, (s >> 16) as u8, 255]
        })
        .collect()
}

fn tile_of(px: &[u8], w: u32, tx: u32, ty: u32) -> Vec<u8> {
    let mut tile = vec![0u8; 4096];
    for r in 0..32u32 {
        let src = (((ty * 32 + r) * w + tx * 32) * 4) as usize;
        tile[(r * 128) as usize..(r * 128 + 128) as usize].copy_from_slice(&px[src..src + 128]);
    }
    tile
}

fn adversarial_tiles() -> Vec<Vec<u8>> {
    (0..8u32)
        .flat_map(|k| (0..4u64).map(move |s| test_pattern(k, s)))
        .collect()
}

fn check_variant(encoder: Cdf53Encoder) {
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    if encoder == Cdf53Encoder::Subgroup && gpu.cdf53_encode_sg.is_none() {
        // Not a skip of the gate: the workgroup test still runs. Say so loudly.
        eprintln!("NOTE: this device has no subgroup CDF 5/3 encode; only the workgroup variant is gated here");
        return;
    }
    let tiles = adversarial_tiles();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let got = gpu.encode_cdf53_tiles_with(encoder, &refs).expect("encode");
    let bad: Vec<usize> = (0..tiles.len())
        .filter(|&i| got[i] != Cdf53Encoded::of_tile(&tiles[i]))
        .collect();
    assert!(bad.is_empty(), "{encoder:?}: tiles differing from the CPU encoder: {bad:?}");
}

#[test]
fn the_workgroup_variant_matches_the_cpu_encoder() {
    check_variant(Cdf53Encoder::Workgroup);
}

#[test]
fn the_subgroup_variant_matches_the_cpu_encoder() {
    check_variant(Cdf53Encoder::Subgroup);
}

#[test]
fn a_full_noise_frame_matches_on_every_listed_tile() {
    let (w, h) = (1920u32, 1088u32);
    let (cols, rows) = (w / 32, h / 32);
    let mut gpu = GpuFrameProcessor::new(cols * rows + 8).expect(NEEDS_GPU);
    let px = noise_frame(w, h, 0x2545_F491_4F6C_DD1D);
    gpu.process_frame_from_pixels(&px, w, h, w * 4).expect("process_frame_from_pixels");
    let count = unsafe { *gpu.cdf53_compact_count_ptr } as usize;
    assert!(
        count >= (cols * rows) as usize * 9 / 10 && count <= (cols * rows) as usize,
        "compact list holds {count} of {} tiles",
        cols * rows
    );
    let list = unsafe { std::slice::from_raw_parts(gpu.cdf53_compact_list_ptr, count) };
    let decoded = unsafe { gpu.cdf53_dirty_out.decode(count) };
    let mut bad = 0usize;
    for (&t, enc) in list.iter().zip(&decoded) {
        let want = Cdf53Encoded::of_tile(&tile_of(&px, w, t % cols, t / cols));
        if enc.as_ref() != Ok(&want) {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "{bad}/{count} listed tiles differ from the CPU encoder ({:?})", gpu.cdf53_encoder);
}

#[test]
fn the_escalation_list_matches_the_cpu_encoder() {
    let (w, h) = (256u32, 128u32);
    let cols = w / 32;
    let mut gpu = GpuFrameProcessor::new((w / 32) * (h / 32) + 8).expect(NEEDS_GPU);
    let px = noise_frame(w, h, 99);
    let candidates: Vec<u32> = vec![0, 3, 9, 17, 31];
    gpu.set_escalation_candidates(&candidates);
    gpu.process_frame_from_pixels(&px, w, h, w * 4).expect("process_frame_from_pixels");
    let decoded = unsafe { gpu.cdf53_escalation_out.decode(candidates.len()) };
    for (&t, enc) in candidates.iter().zip(&decoded) {
        let want = Cdf53Encoded::of_tile(&tile_of(&px, w, t % cols, t / cols));
        assert_eq!(enc.as_ref(), Ok(&want), "escalation tile {t}");
    }
}

/// The start-up self-test must refuse a wrong shader. Build one with the RLE
/// escape threshold off by one, put it in place of the workgroup variant,
/// and run the self-test on it.
#[test]
fn the_self_test_refuses_a_broken_encoder() {
    let src = include_str!("../shaders/cdf53_encode.comp").replace("0x7Fu", "0x7Eu");
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/cdf53-selftest");
    std::fs::create_dir_all(&dir).unwrap();
    let (comp, spv) = (dir.join("broken.comp"), dir.join("broken.spv"));
    std::fs::write(&comp, src).unwrap();
    let out = std::process::Command::new("glslc")
        .args(["-O", "--target-env=vulkan1.1", "-o"])
        .arg(&spv)
        .arg(&comp)
        .output()
        .expect("glslc (needed by this test)");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let words = ash::util::read_spv(&mut std::io::Cursor::new(std::fs::read(&spv).unwrap())).unwrap();

    let mut gpu = GpuFrameProcessor::new(64).expect(NEEDS_GPU);
    unsafe {
        let module = gpu
            .device
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
            .unwrap();
        let (dsl, layout, pipeline) = crate::capture::pipeline_builder::build_compute_pipeline(
            &gpu.device,
            &super::setup::CDF53_ENCODE_BINDINGS,
            12,
            module,
        )
        .unwrap();
        let old = std::mem::replace(
            &mut gpu.cdf53_encode_wg,
            ComputeStage { module, dsl, layout, pipeline },
        );
        old.destroy(&gpu.device);
    }
    let r = gpu.cdf53_self_test(Cdf53Encoder::Workgroup);
    assert!(r.is_err(), "the self-test accepted a broken encoder");
}
```

`super::setup::CDF53_ENCODE_BINDINGS` must be visible. If it is a private `const` in `setup.rs`, make it `pub(super) const`. If `pipeline_builder` is not reachable as `crate::capture::pipeline_builder`, use the path `setup.rs` imports it by. `gpu.device` must be accessible from this child module; check its visibility with `grep -n " device:" ghostframe-lib/src/capture/gpu_pipeline/mod.rs`.

`mod.rs`: rename the module declaration to `mod cdf53_encode_gate_tests;`. `ci.yml`: rename the skip to `--skip gpu_pipeline::cdf53_encode_gate_tests::`, and add `--skip gpu_backend_reencodes_` next to `--skip gpu_backend_pre_encodes_` (Step 6's test needs a GPU too).

- [ ] **Step 2: `fallback_tests.rs`**

`check(n)` compares encodings instead of coefficients:

```rust
fn check(n: usize) {
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let tiles: Vec<Vec<u8>> = (0..n as u64).map(tile).collect();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let got = gpu.encode_cdf53_tiles(&refs).expect("encode failed");
    assert_eq!(got.len(), n);
    let bad = (0..n).filter(|&i| got[i] != Cdf53Encoded::of_tile(&tiles[i])).count();
    assert_eq!(bad, 0, "{bad}/{n} tiles differ from the CPU encoder");
}
```

Rename the three `…_matches_the_cpu_forward_exactly` tests to `…_matches_the_cpu_encoder_exactly`, and update the module doc ("the GPU encode … The oracle is `Cdf53Encoded::of_tile`"). `a_wrongly_sized_tile_is_rejected` calls `encode_cdf53_tiles`. In the `#[ignore]`d round-trip timing test, replace `forward_cdf53_tiles` with `encode_cdf53_tiles`.

- [ ] **Step 3: `tests.rs`**

The three tests that read `cdf53_coefficients_ptr`:
- `dmabuf_real_cdf53_forward_gpu_matches_cpu_reference`
- `cdf53_forward_from_pixels_matches_cpu_reference`
- `cdf53_forward_from_pixels_live_dims_matches_cpu_reference`

Rename them `…_encode_…` in place of `…_forward_…`. In each, replace the block that builds `gpu_coeffs` and the mismatch loop with:

```rust
    let decoded = unsafe { processor.cdf53_dirty_out.decode(1) };
    assert_eq!(
        decoded[0].as_ref(),
        Ok(&Cdf53Encoded::of_tile(&tile_bgra)),
        "GPU encode of tile (0,0) differs from the CPU encoder"
    );
```

Keep each test's `cpu_coefficients` setup only if it is still used; otherwise delete it. Keep their long explanatory comments; change "coefficients" to "encoding" where they describe what is compared.

`golden_surfaces`: replace the CDF 5/3 block (from `// CDF 5/3 forward (L1+L2+L3) for the compact tiles, sorted by tile.` through `rows.push(("cdf53_coeffs", f_cc.0));`), and the escalation block after it, with:

```rust
    // CDF 5/3 encode for the compact tiles, sorted by tile.
    let cn = *p.cdf53_compact_count_ptr as usize;
    stats.cdf_max = stats.cdf_max.max(cn);
    let clist = std::slice::from_raw_parts(p.cdf53_compact_list_ptr, cn);
    let decoded = p.cdf53_dirty_out.decode(cn);
    let mut corder: Vec<usize> = (0..cn).collect();
    corder.sort_by_key(|&c| clist[c]);
    let mut f_cl = Fnv::new();
    let mut f_cp = Fnv::new();
    f_cl.u32(cn as u32);
    for &c in &corder {
        f_cl.u32(clist[c]);
        f_cp.u32(clist[c]);
        let enc = decoded[c].as_ref().expect("cdf53 record");
        if enc.passes.len() > 1 {
            stats.cdf_nonzero_tiles += 1;
        }
        f_cp.u32(enc.present_passes as u32);
        for (pass, bytes) in &enc.passes {
            f_cp.u32(*pass as u32);
            f_cp.bytes(bytes);
        }
    }
    rows.push(("cdf53_list", f_cl.0));
    rows.push(("cdf53_payload", f_cp.0));

    // Escalation encode (candidate order is CPU-defined, so raw order).
    let mut f = Fnv::new();
    f.u32(esc_count as u32);
    if esc_count > 0 {
        let esc = p.cdf53_escalation_out.decode(esc_count);
        if esc.iter().any(|e| e.as_ref().is_ok_and(|e| e.passes.len() > 1)) {
            stats.esc_nonzero += 1;
        }
        for e in &esc {
            let e = e.as_ref().expect("escalation record");
            f.u32(e.present_passes as u32);
            for (pass, bytes) in &e.passes {
                f.u32(*pass as u32);
                f.bytes(bytes);
            }
        }
    }
```

Then keep whatever line followed the old escalation block (it pushes the `"escalation"` row). `cdf_nonzero_tiles` and `esc_nonzero` are non-vacuity counters: "more than pass 0" is the encoded form of "some coefficient is non-zero", so the golden test's non-vacuity assertions keep their meaning.

Regenerating the golden table: first run the gate (Task 9 Step 1). Only once the encode gate is green, run:

```bash
~/work/gf-gpu-run cargo test --release -p ghostframe-lib --lib capture::gpu_pipeline::tests::golden -- --nocapture --test-threads=1
```

It fails and prints the observed rows between `GOLDEN_TABLE_BEGIN`/`END`. **Only rows named `cdf53_coeffs` (now `cdf53_payload`) and `escalation` may change.** Every other row must be identical to `GOLDEN_EXPECTED`; check with a diff before pasting. If any other row differs, stop and report: something outside the encode changed. Paste the new table, re-run, expect PASS.

- [ ] **Step 4: `damage_tests.rs`**

Replace `escalation_coeffs` with:

```rust
/// The escalation encode for `n` candidates, read back.
fn escalation_encoded(gpu: &GpuFrameProcessor, n: usize) -> Vec<Cdf53Encoded> {
    unsafe { gpu.cdf53_escalation_out.decode(n) }
        .into_iter()
        .map(|r| r.expect("escalation record"))
        .collect()
}
```

In `an_unchanged_tick_equals_a_full_compare_of_identical_pixels`:
- the two `escalation_coeffs(…)` calls become `escalation_encoded(…)`; keep the `assert_eq!(we, ge, …)`;
- the CPU check loop becomes:

```rust
            for (k, &tile) in candidates.iter().enumerate() {
                let cpu = Cdf53Encoded::of_tile(&grid.extract_tile(&px, w * 4, tile % cols, tile / cols));
                assert_eq!(
                    ge[k], cpu,
                    "round {round} tick {tick}: tile {tile} encode vs CPU of the screen"
                );
            }
            esc_nonzero += usize::from(ge.iter().any(|e| e.passes.len() > 1));
```

Fix any other `escalation_coeffs` call sites the same way (`grep -n escalation_coeffs damage_tests.rs`).

- [ ] **Step 5: `readback_bench.rs`**

Replace the `// cdf53: per-tile 3072 i32 -> i16, as the production Phase B does` block with:

```rust
                // cdf53: records checked and payloads copied, as production Phase B does
                let t0 = Instant::now();
                ncdf = unsafe { *gpu.cdf53_compact_count_ptr } as usize;
                ncdf = ncdf.min(tiles);
                let decoded = unsafe { gpu.cdf53_dirty_out.decode(ncdf) };
                let keep: usize = decoded
                    .iter()
                    .map(|r| r.as_ref().map_or(0, |e| e.passes.iter().map(|(_, b)| b.len()).sum()))
                    .sum();
                t_cdf.push(t0.elapsed().as_secs_f64() * 1e3);
```

In the `println!`, rename the label `cdf53 coeffs` to `cdf53 payload`.

- [ ] **Step 6: A refused record becomes a new tile (`io_bridge/tests.rs`)**

Add after `gpu_backend_pre_encodes_fallback_tiles_exactly_as_the_cpu_transform`:

```rust
/// A dirty tile whose GPU record is refused is not dropped: it is encoded
/// again from the frame's pixels and emitted as a dirty tile in the same
/// frame, byte-identical to the CPU encoder (owner decision 2026-11-07).
/// Needs a GPU; must not skip.
#[tokio::test]
async fn gpu_backend_reencodes_a_tile_whose_record_was_refused() {
    const W: u32 = 128;
    const H: u32 = 64;
    let _gpu = crate::gpu_test_lock::lock();
    let processor = crate::capture::gpu_pipeline::GpuFrameProcessor::new(64)
        .expect("this test needs a Vulkan GPU and must not skip");
    let mut backend = GpuBackend::new(processor);
    // Noise: every tile has far more than 16 colours, so every tile is in the
    // GPU's own CDF 5/3 compact list on the first frame.
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let pixels: Vec<u8> = (0..W * H)
        .flat_map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            [s as u8, (s >> 8) as u8, (s >> 16) as u8, 255]
        })
        .collect();
    let frame = FrameSubmission {
        width: W,
        height: H,
        stride: W * 4,
        pixels: pixels.clone().into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    };
    backend.analyse(&frame, &[], false).expect("analyse");
    let listed: Vec<u32> = {
        let p = backend.processor_mut();
        let n = unsafe { *p.cdf53_compact_count_ptr } as usize;
        assert!(n >= 2, "fixture: the noise frame must list CDF 5/3 tiles, got {n}");
        let list = unsafe { std::slice::from_raw_parts(p.cdf53_compact_list_ptr, n) }.to_vec();
        p.refuse_cdf53_dirty_record(0);
        list
    };
    let out = backend.finish_tile(
        &frame,
        &TileRequest {
            cols: W / 32,
            rows: H / 32,
            escalation: &[],
            fallback: &[],
        },
    );
    assert_eq!(
        out.cdf53_dirty.len(),
        listed.len(),
        "every listed tile is emitted, the refused one included"
    );
    let grid = crate::tile::TileGrid::new(W, H);
    for &t in &listed {
        let (tx, ty) = (t % (W / 32), t / (W / 32));
        let got = out
            .cdf53_dirty
            .iter()
            .find(|d| (d.tile_x as u32, d.tile_y as u32) == (tx, ty))
            .unwrap_or_else(|| panic!("tile ({tx},{ty}) missing"));
        let want = Cdf53Encoded::of_tile(&grid.extract_tile(&pixels, W * 4, tx, ty));
        assert_eq!(got.encoded, want, "tile ({tx},{ty})");
    }
}
```

Check the hook is load-bearing. Temporarily change the `Err(e) =>` arm of the dirty loop in `finish` so it neither pushes to `refused_dirty` nor logs, only `continue;`, which is the old drop-the-tile behaviour. Run the test and see it FAIL on the length assertion, then restore.

If `cdf53_compact_count_ptr` / `cdf53_compact_list_ptr` are not reachable from `io_bridge::tests` (they are `pub` on `GpuFrameProcessor`, so they should be), report it rather than widening other visibility.

- [ ] **Step 7: Build and run**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings
git grep -n "forward_cdf53_tiles\|cdf53_escalation_coefficients\|cdf53_fallback_coefficients\|coef_read_ns\|coef_encode_ns\|ToWorker::Forward\|escalation_coeffs" -- ':!docs/superpowers'
```
Expected: both clippy runs clean. The grep is empty, or names only `cdf53_spike.rs` if this branch carries it; it must not.

---

### Task 9: GPU runs, mutation proofs, commit

- [ ] **Step 1: The encode gate and its neighbours**

```bash
~/work/gf-gpu-run cargo test --release -p ghostframe-lib --lib -- --test-threads=1 \
    cdf53_encode_gate_tests fallback_tests cdf53_encode_from_pixels dmabuf_real_cdf53 pool_tests \
    cdf53_shader_gen cdf53_encode::tests damage_tests gpu_backend_pre_encodes_ gpu_backend_reencodes_
```
Expected: all pass, including `the_subgroup_variant_matches_the_cpu_encoder` with no NOTE line on the RX 480. Then do the kernel-log check. Then regenerate the golden rows as described in Task 8 Step 3, and run `golden_from_pixels_sequence` green.

- [ ] **Step 2: The workgroup variant through the whole frame**

With `GHOSTFRAME_CDF53_RLE=wg` the daemon would choose `_wg`. The frame-level tests use `new`, which is `Auto`. So run the frame tests once with the subgroup pipeline unavailable: temporarily change `GpuFrameProcessor::new` to call `Self::with_cdf53_rle(max_tiles, crate::config::Cdf53RleChoice::Workgroup)`, then:

```bash
~/work/gf-gpu-run cargo test --release -p ghostframe-lib --lib -- --test-threads=1 \
    cdf53_encode_gate_tests fallback_tests golden_from_pixels_sequence damage_tests
```
Expected: all pass. The golden hashes are the same, because both variants produce the same bytes; this is the test of that. `the_subgroup_variant_matches_the_cpu_encoder` prints its NOTE. Revert `new`.

- [ ] **Step 3: Prove the gates can fail**

For each mutation:
1. apply it;
2. run `just shaders-cdf53`;
3. run the Step 1 command;
4. see the named test FAIL;
5. restore with `git checkout -- ghostframe-lib/src/capture/shaders/` and `just shaders-cdf53`.

Record the failure counts for the PR body.

| Mutation in `cdf53_encode.comp` | Must fail |
|---|---|
| `return v == 0u ? (prev_zero ? 0u : 1u) : (v < 0x7Fu ? 1u : 2u);` → `… (v < 0x7Eu ? 1u : 2u);` | the subgroup and workgroup gate tests, the self-test at start-up (every test that calls `new` errors with "refusing to start") |
| in `emit_word`, `put_byte(pos, 0x80u \| (end - (w * 4u + j) - 1u));` → `… - 0u));` | the gate tests |
| `wd[p] \|= ((m >> (13u - p)) & 1u) << k;` → `((m >> (12u - p)) …` (the shader still compiles; `p = 13` then shifts by `-1`, an out-of-range shift that yields wrong bits) | the gate tests |

The first row is the strongest proof: the self-test catches it, so `GpuFrameProcessor::new` fails everywhere. That is the intended behaviour.

- [ ] **Step 4: Commit Tasks 6–9**

```bash
git status --short
git add ghostframe-lib/src/capture/gpu_pipeline/setup.rs ghostframe-lib/src/capture/gpu_pipeline/mod.rs \
        ghostframe-lib/src/capture/gpu_pipeline/frame.rs ghostframe-lib/src/capture/gpu_pipeline/tests.rs \
        ghostframe-lib/src/capture/gpu_pipeline/damage_tests.rs ghostframe-lib/src/capture/gpu_pipeline/fallback_tests.rs \
        ghostframe-lib/src/capture/gpu_pipeline/readback_bench.rs \
        ghostframe-lib/src/capture/gpu_pipeline/cdf53_encode_gate_tests.rs \
        ghostframe-lib/src/transport/io_bridge.rs ghostframe-lib/src/transport/io_bridge/frame_backend.rs \
        ghostframe-lib/src/transport/io_bridge/frame_worker.rs ghostframe-lib/src/transport/io_bridge/emit.rs \
        ghostframe-lib/src/transport/io_bridge/frame.rs ghostframe-lib/src/transport/io_bridge/unchanged_oracle_tests.rs \
        ghostframe-lib/src/transport/io_bridge/tests.rs \
        .github/workflows/ci.yml
git commit -m "perf(cdf53): encode on the GPU; only the payload comes back

The frame's compact list, the escalation list and the fallback chunk run
cdf53_encode (subgroup or workgroup-memory RLE, chosen by a start-up
self-test against the CPU encoder) and the host checks each record and
cuts the payload into passes. No coefficient readback, no CPU encode;
the coefficient buffer survives only under cdf53-diag.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

`git mv` already staged the rename; `git status` must show nothing unstaged that belongs to this change.

---

### Task 10: Docs, measurements, full gates

- [ ] **Step 1: AGENTS.md**

In "Diagnostic probes", add after the `GHOSTFRAME_NO_COMPUTE_EXPORT` entry:

```markdown
- `GHOSTFRAME_CDF53_RLE=wg` (server) — encode CDF 5/3 with the
  workgroup-memory RLE variant (`cdf53_encode_wg.spv`) instead of the
  subgroup one. Unset (`auto`), the subgroup variant is used where the device
  has 32-lane subgroups (native, or pinned with `VK_EXT_subgroup_size_control`)
  **and** it passes a start-up self-test against the CPU encoder; look for
  `CDF 5/3 GPU encode` with `encoder="subgroup"` or `"workgroup"` in the
  journal. If neither variant passes, the daemon refuses to start and says so.
  The whole encode (forward, bit-planes, RLE, packing) is on the GPU; only
  the payload and a 48-byte record per tile come back. Rebuild the shaders
  with `just shaders-cdf53` after editing `cdf53_encode.comp`.
```

- [ ] **Step 2: The spec**

In `docs/superpowers/specs/2026-11-07-cdf53-gpu-encode-design.md`:
- §4.4: replace the sentence that begins "If `_wg` also fails, there is no GPU CDF 5/3 encode on this device" with: "If `_wg` also fails, `GpuFrameProcessor` construction fails and the daemon refuses to start with the reason, as for a host with no usable GPU."
- §6, step 1: replace "A record failing any check marks that tile's encode as failed, with a rate-limited `warn`. Failed tiles are retried by the commit, as a failed fallback forward is today." with: "A dirty or escalation tile whose record fails a check is treated as a new tile (owner decision 2026-11-07). It is encoded again from the frame's pixels through the fallback encode and emitted in the same frame and list, with a `warn`. Only if that re-encode fails too is the GPU baseline invalidated, so the next frame repaints it. The tile was already committed to the SAD reference, so it is never just left out. A fallback batch with a bad record fails as a whole, and the caller logs, invalidates and retries, as before."
- §4.2 Compaction: replace "The streams are written to the payload buffer, and the record is written last (4.5)." with: "The tile's payload is assembled in workgroup memory, which is free once the bit-planes are out, and copied out as whole words (no 8-bit storage is needed); the record follows (4.5)."

- [ ] **Step 3: Measurements for the PR body**

Run `readback_report` on this branch and on PR 1's branch (or master after PR 1) as in PR 1's Task 7 Step 1. Report:
- `process_frame` and the `cdf53` column for 1080p / 4K × noise / text, before and after;
- the subgroup vs workgroup encode cost: the Step 2 frame tests' wall time is not a measurement, so take the GPU numbers from the spike (`cdf53_plan_shader_timing` on `spike/cdf53-gpu`: 4K noise sg 4.52 ms, wg 6.93 ms) and say they are the spike's.

- [ ] **Step 4: Full gates**

Exactly PR 1's Task 7 Steps 2 and 3:
- the four `ci/check-*.sh`, fmt, `git rebase -x` with both clippy commands;
- lib tests on the GPU;
- client-core tests;
- `just ci-local`;
- every `browserless*` suite in release with `--test-threads=1`;
- the Docker e2e with `GHOSTFRAME_E2E_REQUIRE_VAAPI=1` under the bench lock, reading the exit code.

Also run the e2e CDF 5/3 tests explicitly, since they exercise the decoded wire end to end. They are in `ci/skip-list.txt` because they need the GPU harness, so CI never runs them. With the same lock and `gf-gpu-run`:

```bash
GHOSTFRAME_E2E_REQUIRE_VAAPI=1 ~/work/gf-gpu-run cargo test --test e2e -- --test-threads=1 e2e_cdf53_
```

- [ ] **Step 5: Commit the docs and draft the PR body**

```bash
git add AGENTS.md docs/superpowers/specs/2026-11-07-cdf53-gpu-encode-design.md
git commit -m "docs(cdf53): GHOSTFRAME_CDF53_RLE and the GPU encode's failure handling

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

Write `target/pr2-body.md`, not committed, containing:
- what changed;
- the decisions list at the top of this plan;
- the measurements;
- the mutation table with counts;
- the gate results;
- the golden-row change: only `cdf53_payload` / `escalation`, and why.

End it with:

```
🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

The orchestrator pushes and opens the PR.
