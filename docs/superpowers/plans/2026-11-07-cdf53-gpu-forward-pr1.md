# CDF 5/3 parallel-lifting forward (PR 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the three serial-lifting server shaders (`cdf53_forward_l{1,2,3}.comp`) with one shader. It does all three levels in one dispatch, with one invocation per line, and produces byte-identical coefficients.

**Architecture:** `cdf53_encode.comp` is new. One 96-invocation workgroup per listed tile; generated, fully unrolled lifting functions; workgroup-memory transpose between row and column passes. PR 1 builds only its forward-only variant (`cdf53_forward.spv`), which writes the same i32 coefficient layout to the same buffers as L3 did today. Readback, the CPU encode and every consumer are unchanged. The per-level diagnostics (`GHOSTFRAME_CDF53_SKIP_L2_L3`, `GHOSTFRAME_CDF53_SKIP_L3`, `forward_level{1,2}_only`) only make sense with three dispatches, so they are removed.

**Tech Stack:** Rust (ash/Vulkan 1.1), GLSL 450 compiled with `glslc --target-env=vulkan1.1`, cargo tests through `~/work/gf-gpu-run`.

**Spec:** `docs/superpowers/specs/2026-11-07-cdf53-gpu-encode-design.md` (sections 3, 4.1, 4.2, 5, 8).
**Spike it is ported from:** branch `spike/cdf53-gpu`, `ghostframe-lib/src/capture/shaders/spike/cdf53_fused.comp` and `gen_lift` in `ghostframe-lib/src/capture/gpu_pipeline/cdf53_spike.rs`. They are byte-exact on 61,200 tiles.

---

## Ground rules (read before Task 1)

- **Where to work.** In a new worktree made from `origin/master`. Never build in `~/work/ghostframe`.
  ```bash
  cd ~/work/ghostframe
  git -c credential.helper= -c credential.helper='!gh auth git-credential' fetch -q \
      "https://github.com/$(gh repo view --json nameWithOwner -q .nameWithOwner).git" master:refs/remotes/origin/master
  git worktree add -b perf/cdf53-forward-parallel ~/work/gf-cdf53-fwd origin/master
  cp -a ghostframe-web-client/dist ~/work/gf-cdf53-fwd/ghostframe-web-client/
  cd ~/work/gf-cdf53-fwd
  # The spec and both plans as one docs commit. Take the files, not the spike's
  # commits: those also carry spike-only test code.
  git checkout spike/cdf53-gpu -- docs/superpowers/specs/2026-11-07-cdf53-gpu-encode-design.md \
      docs/superpowers/plans/2026-11-07-cdf53-gpu-forward-pr1.md \
      docs/superpowers/plans/2026-11-07-cdf53-gpu-encode-pr2.md
  git commit -m "docs: CDF 5/3 GPU encode spec and plans

  Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
  ```
  Plain `git fetch` asks for an SSH key that is not there; the HTTPS form above is the one that works here.
- **GPU runs.** Every command that touches the GPU goes through `~/work/gf-gpu-run …`, one at a time, without `GF_ALLOW_VAAPI`. After each one:
  ```bash
  journalctl -k -p warning --since -10min --no-pager | grep -iE 'amdgpu|ring|reset|fault' | grep -v 'PRT request'
  ```
  Empty output is good. Anything else: stop and report.
- **Git.** Never `git add -A`; stage explicit paths. Never push. Commit messages end with:
  ```
  Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
  ```
- **Safety.** Never kill processes. Stop if `df -h /home` > 88 % or `/tmp` > 85 %.
- **What proves correctness.** A green GPU test only counts if it would have failed on broken code; Task 6 proves that for these gates. Treat `cargo clippy --workspace --all-targets -- -D warnings` as the compile gate, not the test summary (AGENTS.md).

## File map

| File | Change |
|---|---|
| `ghostframe-lib/src/capture/shaders/cdf53_encode.comp` | **new**: forward transform, generated lifting block |
| `ghostframe-lib/src/capture/shaders/cdf53_forward.spv` | **new**: `glslc -DCDF53_FORWARD_ONLY=1` build of it |
| `ghostframe-lib/src/capture/gpu_pipeline/cdf53_shader_gen.rs` | **new**, `cfg(test)`: lifting generator, staleness test, regenerate entry |
| `ghostframe-lib/src/capture/gpu_pipeline/cdf53_forward_gate_tests.rs` | **new**, `cfg(test)`: GPU-vs-CPU gate on adversarial tiles and a full noise frame |
| `ghostframe-lib/src/capture/gpu_pipeline/mod.rs` | 3 stages → 1 field, drop, module declarations |
| `ghostframe-lib/src/capture/gpu_pipeline/setup.rs` | 3 pipelines → 1, binding tables, pool sizes |
| `ghostframe-lib/src/capture/gpu_pipeline/frame.rs` | one `run_cdf53_forward_stage` for dirty / escalation / fallback; `StageDispatch::Skip` removed |
| `ghostframe-lib/src/config.rs` | `cdf53_skip_l2_l3` / `cdf53_skip_l3` removed |
| `ghostframe-lib/src/transport/io_bridge/frame.rs` | diag compares against full `forward` only |
| `ghostframe-protocol/src/codec/cdf53.rs` | `forward_level1_only` / `forward_level2_only` removed |
| `ghostframe-e2e/tests/e2e.rs` | `e2e_cdf53_server_gpu_vs_cpu_diff` loses the L1/L2-only options |
| `ci/check-env-reads.sh` | `gpu_pipeline/frame.rs` exemption removed |
| `.github/workflows/ci.yml` | lib-test skip for the new GPU gate module |
| `Justfile` | `shaders-cdf53` recipe |
| `ghostframe-lib/src/capture/shaders/cdf53_forward_l{1,2,3}.{comp,spv}` | **deleted** |

---

### Task 1: The lifting generator and its staleness test

**Files:**
- Create: `ghostframe-lib/src/capture/gpu_pipeline/cdf53_shader_gen.rs`
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/mod.rs` (module list near the other `#[cfg(test)] mod …;` lines, around line 973–996)

- [ ] **Step 1: Write the generator and the failing staleness test**

Create `ghostframe-lib/src/capture/gpu_pipeline/cdf53_shader_gen.rs`:

```rust
//! Generator for the unrolled lifting functions of `cdf53_encode.comp`.
//!
//! The shader holds each line of a 5/3 lifting pass in locals (`v0..v31`),
//! which is what makes one-invocation-per-line fast, and is too long and
//! too regular to maintain by hand. This module writes that block; the
//! shader carries it between the BEGIN/END markers below, checked in, so
//! the build never runs a generator. `generated_lifting_matches_the_checked_in_shader`
//! fails when the two drift; `just shaders-cdf53` regenerates the block
//! (via the ignored `regenerate` test) and recompiles the `.spv`.
//!
//! The lifting is `cdf53::row_forward` (ghostframe-protocol) unrolled:
//! predict odd samples, update even ones, mirror at both ends, then
//! deinterleave (low half first). Rows read consecutive words; columns read
//! every `ROW`-th word of the padded work area.

use std::path::PathBuf;

/// Padded row stride of the shader's work area, in words. The shader
/// declares the same constant; the staleness test checks it.
const ROW: usize = 33;
const SHADER: &str = "src/capture/shaders/cdf53_encode.comp";
const BEGIN: &str = "// BEGIN GENERATED LIFTING (cdf53_shader_gen.rs; `just shaders-cdf53`)\n";
const END: &str = "// END GENERATED LIFTING\n";

/// `lift_rows_{n}` and `lift_cols_{n}` for a line of `n` samples starting
/// at word `base` of the shared array `A`.
fn gen_lift(n: usize) -> String {
    let mut s = String::new();
    for (name, stride) in [("rows", 1usize), ("cols", ROW)] {
        s += &format!("void lift_{name}_{n}(uint base) {{\n");
        for i in 0..n {
            s += &format!("    int v{i} = A[base + {}u];\n", i * stride);
        }
        // Predict: odd samples become high-pass. Right neighbour mirrors.
        for i in (1..n).step_by(2) {
            let r = if i + 1 < n { i + 1 } else { i - 1 };
            s += &format!("    v{i} -= (v{} + v{r}) >> 1;\n", i - 1);
        }
        // Update: even samples become low-pass. Left neighbour mirrors.
        for i in (0..n).step_by(2) {
            let l = if i == 0 { 1 } else { i - 1 };
            s += &format!("    v{i} += (v{l} + v{} + 2) >> 2;\n", i + 1);
        }
        // Deinterleave: L0 L1 .. then H0 H1 ..
        for j in 0..n / 2 {
            s += &format!("    A[base + {}u] = v{};\n", j * stride, 2 * j);
            s += &format!("    A[base + {}u] = v{};\n", (j + n / 2) * stride, 2 * j + 1);
        }
        s += "}\n";
    }
    s
}

fn generated() -> String {
    [32, 16, 8].iter().map(|&n| gen_lift(n)).collect()
}

fn shader_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SHADER)
}

/// `src` with the text between the markers replaced by `body`.
fn splice(src: &str, body: &str) -> String {
    let b = src.find(BEGIN).expect("BEGIN GENERATED LIFTING marker") + BEGIN.len();
    let e = src.find(END).expect("END GENERATED LIFTING marker");
    assert!(b <= e, "lifting markers out of order");
    format!("{}{}{}", &src[..b], body, &src[e..])
}

#[test]
fn generated_lifting_matches_the_checked_in_shader() {
    let src = std::fs::read_to_string(shader_path()).expect("read cdf53_encode.comp");
    assert!(
        src.contains(&format!("const uint ROW = {ROW}u;")),
        "the shader's ROW must equal the generator's ({ROW})"
    );
    assert!(
        src == splice(&src, &generated()),
        "cdf53_encode.comp's lifting block is stale: run `just shaders-cdf53`"
    );
}

/// Rewrites the shader's lifting block. Not a check: `just shaders-cdf53`
/// runs it, then compiles the `.spv`.
#[test]
#[ignore = "rewrites the checked-in shader; run through `just shaders-cdf53`"]
fn regenerate() {
    let p = shader_path();
    let src = std::fs::read_to_string(&p).expect("read cdf53_encode.comp");
    std::fs::write(&p, splice(&src, &generated())).expect("write cdf53_encode.comp");
}
```

In `ghostframe-lib/src/capture/gpu_pipeline/mod.rs`, add next to the other test modules (after `mod fallback_tests;`):

```rust
#[cfg(test)]
mod cdf53_shader_gen;
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p ghostframe-lib --lib cdf53_shader_gen -- --nocapture`
Expected: FAIL in `generated_lifting_matches_the_checked_in_shader` with `read cdf53_encode.comp` (the file does not exist yet).

- [ ] **Step 3: Write the shader with an empty lifting block**

Create `ghostframe-lib/src/capture/shaders/cdf53_encode.comp`, exactly:

```glsl
#version 450
// cdf53_encode.comp -- the CDF 5/3 tile transform on the server GPU.
//
// One workgroup of 96 invocations per listed tile. Invocation l works on
// channel l / 32 (0 = B, 1 = G, 2 = R) and line l % 32 of it. The tile is
// loaded into workgroup memory, then three levels of integer 5/3 lifting
// run with one invocation per line, the whole line held in locals by the
// generated lift_* functions, and one barrier between a row pass and a
// column pass. Bit-exact against `cdf53::forward` (ghostframe-protocol),
// the oracle: same predict/update rounding, same mirror boundary, same
// coefficient layout.
//
// Variants (glslc -D):
//   CDF53_FORWARD_ONLY=1  write CDF53_TOTAL_COEFFS i32 per list slot
//                         (cdf53_forward.spv)
//
// Rebuild after any edit: `just shaders-cdf53` (regenerates the lifting
// block, then compiles every variant).

layout(local_size_x = 96, local_size_y = 1, local_size_z = 1) in;

// Binding 0: the frame, a B8G8R8A8 storage image, read as the old L1 did.
layout(set = 0, binding = 0, rgba8) readonly uniform image2D current_frame;

// Binding 1: tile list; workgroup N transforms tile tile_indices[N].
layout(set = 0, binding = 1, std430) readonly buffer Cdf53TileList {
    uint tile_indices[];
};

// Binding 2: 3 x 1024 i32 per list slot: B, G, R, each in the layout
// LL3 HL3 LH3 HH3 (16 each), HL2 LH2 HH2 (64 each), HL1 LH1 HH1 (256 each).
layout(set = 0, binding = 2, std430) writeonly buffer Cdf53Coefficients {
    int coefficients[];
};

layout(push_constant) uniform PushConstants {
    uint cols;
    uint rows;
};

const uint ROW = 33u;          // padded row stride, words (no bank conflicts on columns)
const uint CHS = 32u * ROW;    // channel stride, words

// The tile, one 32 x 33 plane per channel; transformed in place.
shared int A[3u * CHS];

// BEGIN GENERATED LIFTING (cdf53_shader_gen.rs; `just shaders-cdf53`)
// END GENERATED LIFTING

// Work-area position of coefficient `i` (0..1023) of one channel's layout.
uint lay2pos(uint i) {
    uint y, x;
    if (i < 64u) {
        // LL3, HL3, LH3, HH3: 4 x 4 each.
        uint b = i >> 4, j = i & 15u;
        y = (j >> 2) + ((b >> 1) << 2);
        x = (j & 3u) + ((b & 1u) << 2);
    } else if (i < 256u) {
        // HL2, LH2, HH2: 8 x 8 each.
        uint k = i - 64u;
        uint b = k >> 6, j = k & 63u;
        y = (j >> 3) + (b >= 1u ? 8u : 0u);
        x = (j & 7u) + (b != 1u ? 8u : 0u);
    } else {
        // HL1, LH1, HH1: 16 x 16 each.
        uint k = i - 256u;
        uint b = k >> 8, j = k & 255u;
        y = (j >> 4) + (b >= 1u ? 16u : 0u);
        x = (j & 15u) + (b != 1u ? 16u : 0u);
    }
    return y * ROW + x;
}

void main() {
    const uint l = gl_LocalInvocationIndex;
    const uint ch = l / 32u;
    const uint line = l % 32u;
    const uint slot = gl_WorkGroupID.x;
    const uint tile = tile_indices[slot];
    const uint tx = tile % cols;
    const uint ty = tile / cols;

    // Load: 1024 pixels, coalesced along x; imageLoad of rgba8 returns
    // [0, 1] floats, so round back to the byte.
    for (uint p = l; p < 1024u; p += 96u) {
        uint x = p & 31u, y = p >> 5;
        vec4 px = imageLoad(current_frame, ivec2(int(tx * 32u + x), int(ty * 32u + y)));
        A[0u * CHS + y * ROW + x] = int(round(px.b * 255.0));
        A[1u * CHS + y * ROW + x] = int(round(px.g * 255.0));
        A[2u * CHS + y * ROW + x] = int(round(px.r * 255.0));
    }
    barrier();

    // Three levels; each a row pass then a column pass on the LL quadrant.
    const uint cb = ch * CHS;
    lift_rows_32(cb + line * ROW);
    barrier();
    lift_cols_32(cb + line);
    barrier();
    if (line < 16u) { lift_rows_16(cb + line * ROW); }
    barrier();
    if (line < 16u) { lift_cols_16(cb + line); }
    barrier();
    if (line < 8u) { lift_rows_8(cb + line * ROW); }
    barrier();
    if (line < 8u) { lift_cols_8(cb + line); }
    barrier();

#if CDF53_FORWARD_ONLY
    for (uint u = l; u < 3072u; u += 96u) {
        coefficients[slot * 3072u + u] = A[(u >> 10) * CHS + lay2pos(u & 1023u)];
    }
#else
#error "only the forward-only variant exists (CDF53_FORWARD_ONLY=1)"
#endif
}
```

- [ ] **Step 4: Fill the lifting block, then check the staleness test passes**

```bash
cargo test -p ghostframe-lib --lib cdf53_shader_gen::regenerate -- --ignored
cargo test -p ghostframe-lib --lib cdf53_shader_gen
```
Expected: the first run passes and the shader now holds six functions per size (`lift_rows_32` … `lift_cols_8`) between the markers. The second run reports `test … generated_lifting_matches_the_checked_in_shader ... ok`.

- [ ] **Step 5: Add the `just` recipe and compile the `.spv`**

In `Justfile`, after the `containers-build:` recipe block, add:

```just
# Regenerate cdf53_encode.comp's lifting block and compile its variants.
# The .spv files are checked in; the build never runs glslc. Run after any
# edit to the shader or to gpu_pipeline/cdf53_shader_gen.rs.
shaders-cdf53:
    cargo test -p ghostframe-lib --lib cdf53_shader_gen::regenerate -- --ignored
    glslc -O --target-env=vulkan1.1 -DCDF53_FORWARD_ONLY=1 \
        -o ghostframe-lib/src/capture/shaders/cdf53_forward.spv \
        ghostframe-lib/src/capture/shaders/cdf53_encode.comp
```

Run: `just shaders-cdf53 && spirv-val --target-env vulkan1.1 ghostframe-lib/src/capture/shaders/cdf53_forward.spv && echo VALID`
Expected: `VALID`.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/cdf53_shader_gen.rs \
        ghostframe-lib/src/capture/gpu_pipeline/mod.rs \
        ghostframe-lib/src/capture/shaders/cdf53_encode.comp \
        ghostframe-lib/src/capture/shaders/cdf53_forward.spv \
        Justfile
git commit -m "feat(cdf53): one-dispatch parallel-lifting forward shader, generated lifting

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 2: The GPU gate (written against today's kernels first)

These tests pass on the current L1/L2/L3 kernels. That is intended: they fix the contract before the swap, and Task 6 proves they can fail.

**Files:**
- Create: `ghostframe-lib/src/capture/gpu_pipeline/cdf53_forward_gate_tests.rs`
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/mod.rs` (test module list)
- Modify: `.github/workflows/ci.yml` (the `cargo test --workspace --lib` line, line 127)

- [ ] **Step 1: Write the gate**

Create `ghostframe-lib/src/capture/gpu_pipeline/cdf53_forward_gate_tests.rs`:

```rust
//! The server GPU's CDF 5/3 forward against the CPU oracle, `cdf53::forward`,
//! exactly. Two entry points: `forward_cdf53_tiles` (the escalation/fallback
//! dispatch) on adversarial tiles, and a full noise frame through
//! `process_frame_from_pixels` (the frame's own compact list, indirect
//! dispatch). Needs a Vulkan GPU; CI skips this module by name.

use super::*;
use crate::encoder::cdf53;

const NEEDS_GPU: &str =
    "GpuFrameProcessor::new failed: this test needs a Vulkan GPU and must not skip";

/// One 32x32 BGRA tile of pattern `kind` (0..=7), with `seed` varying the noise.
/// The patterns hit the transform's edges: flat extremes, maximal
/// high-pass energy (checkerboard, stripes), a lone corner impulse
/// (mirror boundary), full-range noise and a gradient.
fn pattern(kind: u32, seed: u64) -> Vec<u8> {
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

#[test]
fn adversarial_tiles_match_the_cpu_forward_exactly() {
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let tiles: Vec<Vec<u8>> = (0..8u32)
        .flat_map(|k| (0..4u64).map(move |s| pattern(k, s)))
        .collect();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let got = gpu.forward_cdf53_tiles(&refs).expect("forward_cdf53_tiles");
    let n = cdf53::CDF53_TOTAL_COEFFS;
    assert_eq!(got.len(), tiles.len() * n);
    let bad: Vec<usize> = (0..tiles.len())
        .filter(|&i| got[i * n..(i + 1) * n] != cdf53::forward(&tiles[i])[..])
        .collect();
    assert!(bad.is_empty(), "tiles differing from the CPU forward: {bad:?}");
}

#[test]
fn a_full_noise_frame_matches_the_cpu_forward_on_every_listed_tile() {
    let (w, h) = (1920u32, 1088u32);
    let (cols, rows) = (w / 32, h / 32);
    let mut gpu = GpuFrameProcessor::new(cols * rows + 8).expect(NEEDS_GPU);
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let px: Vec<u8> = (0..w * h)
        .flat_map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            [s as u8, (s >> 8) as u8, (s >> 16) as u8, 255]
        })
        .collect();
    gpu.process_frame_from_pixels(&px, w, h, w * 4)
        .expect("process_frame_from_pixels");
    let count = unsafe { *gpu.cdf53_compact_count_ptr } as usize;
    // Noise has far more than 16 colours per tile, so nearly every tile is
    // listed. The exact count is the classifier's business; the gate is
    // that every listed tile is exact, and that the list is not tiny.
    assert!(
        count >= (cols * rows) as usize * 9 / 10 && count <= (cols * rows) as usize,
        "compact list holds {count} of {} tiles",
        cols * rows
    );
    let list = unsafe { std::slice::from_raw_parts(gpu.cdf53_compact_list_ptr, count) };
    let n = cdf53::CDF53_TOTAL_COEFFS;
    let mut bad = 0usize;
    for (slot, &t) in list.iter().enumerate() {
        let (tx, ty) = (t % cols, t / cols);
        let mut tile = vec![0u8; 4096];
        for r in 0..32u32 {
            let src = (((ty * 32 + r) * w + tx * 32) * 4) as usize;
            tile[(r * 128) as usize..(r * 128 + 128) as usize]
                .copy_from_slice(&px[src..src + 128]);
        }
        let have = unsafe { std::slice::from_raw_parts(gpu.cdf53_coefficients_ptr.add(slot * n), n) };
        let want = cdf53::forward(&tile);
        if have.iter().zip(&want).any(|(&g, &c)| g as i16 != c) {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "{bad}/{count} listed tiles differ from the CPU forward");
}
```

In `mod.rs`, add after `mod cdf53_shader_gen;`:

```rust
#[cfg(test)]
mod cdf53_forward_gate_tests;
```

In `.github/workflows/ci.yml`, on the `cargo test --workspace --lib` line (127), append `--skip gpu_pipeline::cdf53_forward_gate_tests::` inside the quoted command, after `--skip gpu_pipeline::fallback_tests::`. CI exempts itself in the workflow; the test never self-skips.

- [ ] **Step 2: Run the gate on today's kernels**

Run:
```bash
~/work/gf-gpu-run cargo test --release -p ghostframe-lib --lib cdf53_forward_gate_tests -- --test-threads=1
```
Expected: 2 passed. Then do the kernel-log check from the ground rules.

If `cdf53_compact_list_ptr` or `cdf53_compact_count_ptr` do not compile from this module, find their declarations in `mod.rs` (`grep -n "cdf53_compact_list_ptr\|cdf53_compact_count_ptr" ghostframe-lib/src/capture/gpu_pipeline/mod.rs`) and use them as `frame_backend.rs` does around line 673. Do not change their visibility.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/cdf53_forward_gate_tests.rs \
        ghostframe-lib/src/capture/gpu_pipeline/mod.rs .github/workflows/ci.yml
git commit -m "test(cdf53): GPU forward gate on adversarial tiles and a full noise frame

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Remove the per-level diagnostics

The three-dispatch forward can stop after L1 or L2 and the CPU compares that to `forward_level{1,2}_only`. The one-dispatch forward cannot stop between levels, so these go before the swap. `GHOSTFRAME_CDF53_DIFF_TILE` (the full-forward diff) stays.

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/frame.rs` (`run_cdf53_forward_l2_stage`, `run_cdf53_forward_l3_stage`, `StageDispatch`)
- Modify: `ghostframe-lib/src/config.rs` (`DiagnosticsConfig`)
- Modify: `ghostframe-lib/src/transport/io_bridge/frame.rs` (~line 1880)
- Modify: `ghostframe-protocol/src/codec/cdf53.rs` (`forward_level1_only`, `forward_level2_only`)
- Modify: `ghostframe-e2e/tests/e2e.rs` (`e2e_cdf53_server_gpu_vs_cpu_diff`)
- Modify: `ci/check-env-reads.sh`

- [ ] **Step 1: GPU stages always dispatch**

In `ghostframe-lib/src/capture/gpu_pipeline/frame.rs`, `run_cdf53_forward_l2_stage`: delete the `#[cfg(feature = "cdf53-diag")] let skip_l2 = …`, the `#[cfg(not(...))] let skip_l2 = false;` and the `let dispatch = if skip_l2 {…}` lines, and their comment. Pass `StageDispatch::Indirect(self.cdf53_dispatch_args.buffer)` directly to `record_stage`. Do the same in `run_cdf53_forward_l3_stage` (`skip_l3`). These functions are deleted in Task 4. This step only removes the env reads, so the tree compiles at every commit.

Then remove the `Skip` variant: in the `enum StageDispatch` (around line 85), delete `Skip,`, and replace the doc line `/// How \`GpuFrameProcessor::record_stage\` dispatches. \`Skip\` still binds` / `/// and pushes (used by the cdf53-diag skip logic).` with:

```rust
/// How `GpuFrameProcessor::record_stage` dispatches.
```

In `record_stage`'s `match dispatch` (around line 1928), delete the arm `StageDispatch::Skip => {}`.

- [ ] **Step 2: Config**

In `ghostframe-lib/src/config.rs`, `DiagnosticsConfig`:
- Delete the fields `cdf53_skip_l2_l3` and `cdf53_skip_l3` with their doc comments.
- In `from_lookup`, delete their two `#[cfg]` initialiser pairs and the comment above them that begins `` // `.is_some()`, not a value comparison``.
- In `from_lookup`'s doc comment, change `` `cdf53_diff_tile` /`` `` `cdf53_dump_pending` / `cdf53_skip_l2_l3` / `cdf53_skip_l3` only`` to `` `cdf53_diff_tile` /`` `` `cdf53_dump_pending` only``.
- In the defaults test (around line 1030), delete `assert!(!cfg.diagnostics.cdf53_skip_l2_l3);` and `assert!(!cfg.diagnostics.cdf53_skip_l3);`.
- Rename the test `diagnostics_config_parses_cdf53_dump_pending_and_skip_gates` to `diagnostics_config_parses_cdf53_dump_pending`, and delete every assertion in it that mentions `SKIP_L2_L3`, `SKIP_L3`, `cdf53_skip_l2_l3` or `cdf53_skip_l3`, with their comments. Keep the two `cdf53_dump_pending` assertions.

- [ ] **Step 3: The diag call site**

In `ghostframe-lib/src/transport/io_bridge/frame.rs`, around line 1878, replace:

```rust
                    // Pick CPU reference based on which dispatches were skipped:
                    // SKIP_L2_L3 → L1-only; SKIP_L3 → L2-only; otherwise full.
                    let skip_l2_l3 = self.diagnostics.cdf53_skip_l2_l3;
                    let skip_l3 = self.diagnostics.cdf53_skip_l3;
                    let cpu_coeffs = if skip_l2_l3 {
                        crate::encoder::cdf53::forward_level1_only(&tile_bgra)
                    } else if skip_l3 {
                        crate::encoder::cdf53::forward_level2_only(&tile_bgra)
                    } else {
                        crate::encoder::cdf53::forward(&tile_bgra)
                    };
```

with:

```rust
                    let cpu_coeffs = crate::encoder::cdf53::forward(&tile_bgra);
```

- [ ] **Step 4: Protocol**

In `ghostframe-protocol/src/codec/cdf53.rs`, delete `forward_level1_only` and `forward_level2_only` entirely: the doc comments, the `#[cfg(feature = "cdf53-diag")]` attributes and the bodies. Then check whether the `cdf53-diag` feature is still used in that crate:

```bash
grep -n "cdf53-diag" ghostframe-protocol/src -r; grep -n "cdf53-diag" ghostframe-protocol/Cargo.toml
```

If nothing in `ghostframe-protocol/src` uses it any more, leave the `Cargo.toml` feature in place. `ghostframe-lib`'s `cdf53-diag = ["ghostframe-protocol/cdf53-diag"]` still names it, and an empty feature is harmless. Removing it is out of scope.

- [ ] **Step 5: The e2e diagnostic test**

In `ghostframe-e2e/tests/e2e.rs`, `e2e_cdf53_server_gpu_vs_cpu_diff`:
- Delete the comment block starting `// Per env-var: when GHOSTFRAME_CDF53_VERIFY_L1_ONLY=1`.
- Delete the two `let verify_l…_only = std::env::var(…)` lines.
- Delete the `if verify_l1_only { … } else if verify_l2_only { … }` block.
- Change `let mut env: Vec<(&str, &str)> = vec![` to `let env: Vec<(&str, &str)> = vec![`.

Keep the rest of the test.

- [ ] **Step 6: The env-read guard**

In `ci/check-env-reads.sh`, delete these comment lines:

```bash
# ghostframe-lib. gpu_pipeline/frame.rs is exempt: its three cdf53-diag reads
# sit in the Vulkan dispatch path, where threading config through is a much
# larger change. They are feature-gated and race nothing; tracked as deferred
# work in docs/superpowers/plans/2026-09-08-lib-config.md.
```

and the filter line `    | drop '^ghostframe-lib/src/capture/gpu_pipeline/frame.rs' \`. Put this one-line comment above `hits=$(scan 'env::var' ghostframe-lib/src \`:

```bash
# ghostframe-lib: config.rs only.
```

- [ ] **Step 7: Verify**

```bash
bash ci/check-env-reads.sh && echo ENV-OK
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings
cargo test -p ghostframe-lib --lib config:: 
cargo test -p ghostframe-protocol --lib cdf53
```
Expected: `ENV-OK`, both clippy runs clean, tests pass. Also check that nothing still references the removed names:

```bash
git grep -n "SKIP_L2_L3\|SKIP_L3\|forward_level1_only\|forward_level2_only\|cdf53_skip_l\|VERIFY_L1_ONLY\|VERIFY_L2_ONLY\|StageDispatch::Skip" -- ':!docs/superpowers'
```
Expected: no output.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/frame.rs ghostframe-lib/src/config.rs \
        ghostframe-lib/src/transport/io_bridge/frame.rs ghostframe-protocol/src/codec/cdf53.rs \
        ghostframe-e2e/tests/e2e.rs ci/check-env-reads.sh
git commit -m "refactor(cdf53): drop the per-level forward diagnostics

They stop the three-dispatch forward after L1 or L2; the one-dispatch
forward has no such point. GHOSTFRAME_CDF53_DIFF_TILE (full forward)
stays, and gpu_pipeline/frame.rs no longer needs its env-read exemption.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Swap the three stages for one

One compile unit: setup, struct, recording and drop change together.

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/setup.rs`
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/mod.rs`
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/frame.rs`

- [ ] **Step 1: Binding tables and pool sizing (`setup.rs`)**

Replace the three tables `CDF53_FORWARD_L1_BINDINGS`, `CDF53_FORWARD_L2_BINDINGS` and `CDF53_FORWARD_L3_BINDINGS` with:

```rust
const CDF53_FORWARD_BINDINGS: [BindingSpec; 3] = [
    BindingSpec::storage_image(0),
    BindingSpec::storage_buffer(1),
    BindingSpec::storage_buffer(2),
];
```

In `STAGE_POOL_USES`, change the array length from `14` to `12`. Replace the three `(&CDF53_FORWARD_L{1,2,3}_BINDINGS, 2),` entries with:

```rust
    (&CDF53_FORWARD_BINDINGS, 2),
```

The doc comment above it says "The cdf53 forward stages run twice (refinement and escalation)". Make it "The cdf53 forward stage runs twice (refinement and escalation)".

In `pool_tests::pool_sizes_follow_the_stage_binding_table`, the L2/L3 tables each took 2 buffers × 2 uses and 2 sets. So the expected values become:

```rust
        assert_eq!(POOL_NEED, (7, 33, 13));
        assert_eq!(
            (POOL_STORAGE_IMAGES, POOL_STORAGE_BUFFERS, POOL_MAX_SETS),
            (8, 33, 16)
        );
```

- [ ] **Step 2: Pipeline creation (`setup.rs`)**

Replace the three blocks from `// --- Cdf53 forward L1 shader module (Stage 4c-L1) ---` through the end of the L3 `build_compute_pipeline(...)?;` with:

```rust
        // --- Cdf53 forward (Stage 4c): all three levels, one dispatch ---
        // `cdf53_encode.comp` built with CDF53_FORWARD_ONLY=1 (`just shaders-cdf53`).
        let cdf53_forward_spv = include_bytes!("../shaders/cdf53_forward.spv");
        let cdf53_forward_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(cdf53_forward_spv.as_slice()))?;
        let cdf53_forward_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&cdf53_forward_spv_words);
        let cdf53_forward_shader_module =
            device.create_shader_module(&cdf53_forward_shader_ci, None)?;

        // Bindings:
        //   0: STORAGE_IMAGE  (current_frame, read-only)
        //   1: STORAGE_BUFFER (tile list, read-only)
        //   2: STORAGE_BUFFER (coefficients, write-only)
        // Push constants: 2 × u32 = 8 bytes (cols, rows).
        let (
            cdf53_forward_descriptor_set_layout,
            cdf53_forward_pipeline_layout,
            cdf53_forward_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &CDF53_FORWARD_BINDINGS,
            8,
            cdf53_forward_shader_module,
        )?;
```

In the struct initialiser (around line 993), replace the three `cdf53_forward_l{1,2,3}: ComputeStage { … },` entries with:

```rust
            cdf53_forward: ComputeStage {
                module: cdf53_forward_shader_module,
                pipeline: cdf53_forward_pipeline,
                layout: cdf53_forward_pipeline_layout,
                dsl: cdf53_forward_descriptor_set_layout,
            },
```

- [ ] **Step 3: Struct and drop (`mod.rs`)**

Replace the three fields (around line 364–371) with:

```rust
    // Cdf53 forward pipeline (Stage 4c): all three levels in one dispatch,
    // `cdf53_encode.comp` built with CDF53_FORWARD_ONLY=1.
    cdf53_forward: ComputeStage,
```

In `Drop` (around line 927–934), replace the three destroy blocks with:

```rust
            // Cdf53 forward pipeline (Stage 4c)
            self.cdf53_forward.destroy(&self.device);
```

In the comment above `cdf53_escalation_list` (around line 345), change `the cdf53_forward_l{1,2,3} shaders reads them` to `the cdf53_forward shader reads them`.

- [ ] **Step 4: One recording function (`frame.rs`)**

Delete `run_cdf53_forward_l1_stage`, `run_cdf53_forward_l2_stage`, `run_cdf53_forward_l3_stage` and `run_cdf53_forward_escalation_stages`, with their doc comments. Add in their place:

```rust
    /// Stage 4c — CDF 5/3 forward wavelet, all three levels in one dispatch.
    ///
    /// One 96-invocation workgroup per list slot (`cdf53_encode.comp`,
    /// forward-only variant): reads the frame image and `list_buffer`,
    /// writes `CDF53_TOTAL_COEFFS` i32 coefficients per slot into
    /// `coeff_buffer`, in the layout `cdf53::forward` produces. Used for the
    /// frame's compact list (indirect), the escalation list and the fallback
    /// tiles (direct).
    ///
    /// Caller must:
    ///   - Ensure `current_view` is in `GENERAL` layout.
    ///   - For `StageDispatch::Indirect`, ensure the args buffer is barriered
    ///     for `INDIRECT_COMMAND_READ`.
    ///   - Add a SHADER_WRITE → HOST_READ barrier on `coeff_buffer` before
    ///     the host reads it.
    ///   - Hold the returned guard alive until after `wait_for_fences`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn run_cdf53_forward_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        current_view: vk::ImageView,
        cols: u32,
        rows: u32,
        list_buffer: vk::Buffer,
        coeff_buffer: vk::Buffer,
        dispatch: StageDispatch<'_>,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        self.record_stage(
            cmd,
            &self.cdf53_forward,
            &[
                StageBinding::Image(current_view),
                StageBinding::Buffer(list_buffer),
                StageBinding::Buffer(coeff_buffer),
            ],
            &[cols, rows],
            dispatch,
        )
    }
```

In `record_shared_stages`, replace the span that starts at the `// ====…` line directly above `// Stage 4c: cdf53_forward L1 → L2 → L3 (per-tile wavelet transform)`. It ends just before the `// ====…` line directly above `// Stage 2a: palette_fold`, and that line is not included. The span covers the L1/L2/L3 calls, their barriers and the whole Stage 4d escalation block. Replace it with:

```rust
        // ============================================================
        // Stage 4c: cdf53_forward, all three levels in one dispatch
        // ============================================================
        let cdf53_forward_ds_guard = self.run_cdf53_forward_stage(
            cmd,
            current_view,
            cols,
            rows,
            self.cdf53_compact_list_buffer,
            self.cdf53_coefficients_buffer,
            StageDispatch::Indirect(self.cdf53_dispatch_args.buffer),
        )?;

        // Barrier → HOST_READ: Phase B reads the coefficient buffer after the fence.
        let buf_barrier_forward_host = buf_barrier(
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
            std::slice::from_ref(&buf_barrier_forward_host),
            &[],
        );

        // ============================================================
        // Stage 4d: cdf53_forward escalation (idle-escalation candidates)
        // ============================================================
        let cdf53_escalation_ds_guard = if escalation_count > 0 {
            let guard = self.run_cdf53_forward_stage(
                cmd,
                current_view,
                cols,
                rows,
                self.cdf53_escalation_list.buffer,
                self.cdf53_escalation_coefficients_buffer,
                StageDispatch::Direct(escalation_count, 1, 1),
            )?;
            // → HOST_READ so Phase B can read after the fence.
            let buf_barrier_esc_host = buf_barrier(
                self.cdf53_escalation_coefficients_buffer,
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
                std::slice::from_ref(&buf_barrier_esc_host),
                &[],
            );
            Some(guard)
        } else {
            None
        };

```

In the `Ok(SharedStageGuards { … })` at the end of `record_shared_stages`, replace the lines `cdf53_forward_l1_ds_guard,`, `cdf53_forward_l2_ds_guard,`, `cdf53_forward_l3_ds_guard,` and `cdf53_escalation_ds_guards,` with:

```rust
            cdf53_forward_ds_guard,
            cdf53_escalation_ds_guard,
```

- [ ] **Step 5: `SharedStageGuards`**

In `struct SharedStageGuards`, replace the field `cdf53_escalation_ds_guards: Option<(…, …, …)>,` and the three `cdf53_forward_l{3,2,1}_ds_guard` fields with the following. It keeps reverse creation order: escalation was created after forward.

```rust
    cdf53_escalation_ds_guard: Option<ScopedDescriptorSets<'a>>,
    cdf53_forward_ds_guard: ScopedDescriptorSets<'a>,
```

In `release_forward`, the destructuring pattern loses `cdf53_forward_l1_ds_guard, cdf53_forward_l2_ds_guard, cdf53_forward_l3_ds_guard, cdf53_escalation_ds_guards,` and gains `cdf53_forward_ds_guard, cdf53_escalation_ds_guard,`. The drops become:

```rust
        drop(cdf53_indirect_args_ds_guard);
        drop(cdf53_forward_ds_guard);
        drop(cdf53_escalation_ds_guard);
        drop(palette_fold_ds_guard);
```

They replace `drop(cdf53_forward_l1_ds_guard); drop(cdf53_forward_l2_ds_guard); drop(cdf53_forward_l3_ds_guard); drop(cdf53_escalation_ds_guards);`.

In `release_reverse`, same pattern change; the drops become:

```rust
        drop(palette_fold_ds_guard);
        drop(cdf53_escalation_ds_guard);
        drop(cdf53_forward_ds_guard);
        drop(cdf53_indirect_args_ds_guard);
```

Its doc line "The escalation triple still frees l1, l2, l3 as a tuple." is now false; delete it.

In the long step comment near line 1003 (`//    cdf53_forward_l1/l2/l3_ds_guard, _cdf53_escalation_ds_guards,`), change that line to:

```rust
        //    cdf53_forward_ds_guard, cdf53_escalation_ds_guard,
```

- [ ] **Step 6: The fallback chunk**

In `forward_cdf53_chunk`, replace:

```rust
            let guards = self.run_cdf53_forward_escalation_stages(
                cmd,
                current.view,
                cols,
                rows,
                n as u32,
                self.cdf53_fallback_list.buffer,
                self.cdf53_fallback_coefficients.buffer,
            )?;
```

with:

```rust
            let guard = self.run_cdf53_forward_stage(
                cmd,
                current.view,
                cols,
                rows,
                self.cdf53_fallback_list.buffer,
                self.cdf53_fallback_coefficients.buffer,
                StageDispatch::Direct(n as u32, 1, 1),
            )?;
```

Further down, replace `drop(guards);` with `drop(guard);`.

In `forward_cdf53_tiles`' doc comment, change "so the existing forward shaders run unchanged with an identity tile list" to "so the forward shader runs with an identity tile list".

- [ ] **Step 7: Compile and lint**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings
git grep -n "cdf53_forward_l[123]\|run_cdf53_forward_escalation_stages\|cdf53_escalation_ds_guards" -- ':!docs/superpowers'
```
Expected: clippy clean twice. The grep lists only the old `.comp`/`.spv` filenames, if anything; they are deleted in Task 5.

- [ ] **Step 8: Run every GPU forward test**

```bash
~/work/gf-gpu-run cargo test --release -p ghostframe-lib --lib -- --test-threads=1 \
    cdf53_forward_gate_tests fallback_tests cdf53_forward_from_pixels dmabuf_real_cdf53 pool_tests cdf53_shader_gen \
    golden_from_pixels_sequence damage_tests
```
Expected: every listed test passes. `dmabuf_real_*` may print a skip line when no DMA-BUF import is possible; note it, it is not a failure. Then do the kernel-log check.

`golden_from_pixels_sequence` matters most here. It hashes every GPU output, including the `cdf53_coeffs` and `escalation` coefficient rows, against the checked-in `GOLDEN_EXPECTED` table in `tests.rs`. This PR must leave the coefficients byte-identical, so **the table must not change**. If the test fails, the new shader is wrong. Do not regenerate the table.

- [ ] **Step 9: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/setup.rs ghostframe-lib/src/capture/gpu_pipeline/mod.rs \
        ghostframe-lib/src/capture/gpu_pipeline/frame.rs
git commit -m "perf(cdf53): forward all three levels in one parallel-lifting dispatch

Replaces cdf53_forward_l{1,2,3}: one 96-invocation workgroup per tile,
one invocation per line with the line in registers, three levels in
workgroup memory. Same coefficients, buffers and consumers.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Delete the old shaders

**Files:**
- Delete: `ghostframe-lib/src/capture/shaders/cdf53_forward_l1.comp`, `…_l1.spv`, `…_l2.comp`, `…_l2.spv`, `…_l3.comp`, `…_l3.spv`

- [ ] **Step 1: Delete and check nothing refers to them**

```bash
git rm ghostframe-lib/src/capture/shaders/cdf53_forward_l1.comp ghostframe-lib/src/capture/shaders/cdf53_forward_l1.spv \
       ghostframe-lib/src/capture/shaders/cdf53_forward_l2.comp ghostframe-lib/src/capture/shaders/cdf53_forward_l2.spv \
       ghostframe-lib/src/capture/shaders/cdf53_forward_l3.comp ghostframe-lib/src/capture/shaders/cdf53_forward_l3.spv
git grep -n "cdf53_forward_l[123]" -- ':!docs/superpowers'
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: the grep prints nothing; clippy clean.

Comments elsewhere may name `cdf53_forward_l1` etc. without the extension (e.g. `cdf53_compact.comp`, `cdf53_indirect_args.comp`). Repoint any the grep finds to `cdf53_encode.comp (cdf53_forward.spv)`. If a `.comp` changes, rebuild its `.spv` with `glslc -O --target-env=vulkan1.1 -o <x>.spv <x>.comp`.

- [ ] **Step 2: Commit**

```bash
git commit -m "chore(cdf53): delete the three-stage forward shaders

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Prove the gate can fail

No commit: this task leaves the tree as it found it.

- [ ] **Step 1: Break the rounding**

In `cdf53_shader_gen.rs`, `gen_lift`, change `"    v{i} += (v{l} + v{} + 2) >> 2;\n"` to `"    v{i} += (v{l} + v{} + 1) >> 2;\n"`. Then:

```bash
just shaders-cdf53
~/work/gf-gpu-run cargo test --release -p ghostframe-lib --lib -- --test-threads=1 cdf53_forward_gate_tests fallback_tests golden_from_pixels_sequence
```
Expected: FAIL in all three groups:
- `adversarial_tiles_match_the_cpu_forward_exactly` lists differing tiles;
- `a_full_noise_frame…` reports a non-zero count;
- the `fallback_tests` fail;
- `golden_from_pixels_sequence` reports mismatching `cdf53_coeffs` / `escalation` rows.

Record the counts for the PR body.

- [ ] **Step 2: Break the layout**

Restore the rounding: `git checkout ghostframe-lib/src/capture/gpu_pipeline/cdf53_shader_gen.rs && just shaders-cdf53`. Then in `cdf53_encode.comp`, `lay2pos`, change the HL1/LH1/HH1 branch's `x = (j & 15u) + (b != 1u ? 16u : 0u);` to `x = (j & 15u) + (b == 1u ? 16u : 0u);`, then `just shaders-cdf53` and run the same command.
Expected: FAIL in both modules.

- [ ] **Step 3: Restore and confirm clean**

```bash
git checkout ghostframe-lib/src/capture/shaders/cdf53_encode.comp ghostframe-lib/src/capture/shaders/cdf53_forward.spv
git status --short
~/work/gf-gpu-run cargo test --release -p ghostframe-lib --lib -- --test-threads=1 cdf53_forward_gate_tests fallback_tests cdf53_shader_gen
```
Expected: `git status` shows no modified tracked files, and the tests pass. Then do the kernel-log check.

---

### Task 7: Measure, run the full gates, write the PR body

- [ ] **Step 1: Before/after numbers**

The existing `readback_report` times `process_frame` for 1080p and 4K, noise (all CDF 5/3) and text. Run it twice:
- in this worktree (after);
- in `~/work/gf-cdf53-gpu` (before). Branch `spike/cdf53-gpu` is `origin/master` plus test-only spike files and docs, so its production code is master's.

```bash
~/work/gf-gpu-run cargo test --release -p ghostframe-lib --lib readback_report -- --ignored --nocapture --test-threads=1 2>&1 | grep READBACK
```
Expected: the `noise (all Cdf53)` lines' `process_frame` median drops by roughly the L1+L2+L3 cost the spike measured (12.5 ms at 1080p, ~46 ms at 4K). Keep both outputs for the PR body.

- [ ] **Step 2: Full local gates**

Run each and keep the outputs:

```bash
for s in ci/check-*.sh; do bash "$s" || echo "FAILED $s"; done
cargo fmt --all -- --check
git rebase -x 'cargo clippy --workspace --all-targets -- -D warnings && cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings' origin/master
~/work/gf-gpu-run cargo test --workspace --lib -- --skip dmabuf_real_ --skip gpu_backend_pre_encodes_ --skip invalidate_baseline_keeps_the_longer_cushion --skip h264_direct_
cargo test -p ghostframe-client-core
just ci-local
for t in browserless browserless_blocked_path browserless_burst_loss browserless_bwe_capacity browserless_bwe_rate_source browserless_determinism browserless_h264 browserless_loop_stall browserless_model_controller browserless_nested_cc browserless_overload browserless_queue_latency; do
  cargo test --release -p ghostframe-e2e --test $t -- --test-threads=1 || echo "FAILED $t"
done
```

Expected: no `FAILED`, fmt clean, the rebase `-x` passes on every commit, and `just ci-local` green, which includes the cbindgen header diff. The handoff lists the browserless suites as a gate. The list above is every `ghostframe-e2e/tests/browserless*.rs`; `ls ghostframe-e2e/tests/browserless*.rs` tells you if one was added since. Capture a failing test's name before any re-run (AGENTS.md "Known flakes").

- [ ] **Step 3: Docker e2e (server path changed)**

Hold the bench lock around the image build and the whole run:

```bash
flock -o ~/work/.gf-bench.lock bash -c 'just containers-build && GHOSTFRAME_E2E_REQUIRE_VAAPI=1 ~/work/gf-gpu-run just test-e2e' 2>&1 | tee target/e2e-pr1.log; echo "exit ${PIPESTATUS[0]}"
```

Expected: `exit 0`. Read the exit code, not the tail: piping masks failures. Then do the kernel-log check.

- [ ] **Step 4: PR body draft**

Write `target/pr1-body.md`, not committed, for the orchestrator. Include:
- what changed;
- the before/after table from Step 1;
- the Task 6 mutation results;
- the gate list with results;
- the removed diagnostics (`GHOSTFRAME_CDF53_SKIP_L2_L3`, `GHOSTFRAME_CDF53_SKIP_L3`, the e2e `VERIFY_L{1,2}_ONLY` options);
- that the spec and plans are included.

End it with:

```
🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

The orchestrator pushes and opens the PR; agents never push.
