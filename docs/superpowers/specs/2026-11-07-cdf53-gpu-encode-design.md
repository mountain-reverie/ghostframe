# CDF 5/3 encode entirely on the server GPU: design

Date: 2026-11-07. Status: design, owner-approved approach (two PRs, "approach A").
Spike: branch `spike/cdf53-gpu`, `ghostframe-lib/src/capture/gpu_pipeline/cdf53_spike.rs` and
`ghostframe-lib/src/capture/shaders/spike/cdf53_fused.comp` (commits b03a3005, fab8b9b4).
Client counterpart (GPU RLE decode on Mali) is a separate spike, out of scope here.

## 1. Goal and requirements

Everything in the CDF 5/3 tile encode runs on the server GPU. Only the final wire payload comes back to the host. The CPU
is left for the user's own work (compilers, developer tools).

Hard requirements:
- **Lossless, bit-exact.** The GPU output equals the CPU oracle (`ghostframe_protocol::codec::cdf53::forward` followed by
  `encode_passes_sparse`) byte for byte, on every tile. There is no tolerance and no float.
- **Wire format unchanged.** Passes, `present_passes`, RLE tokens and framing stay as `encode_passes_sparse` defines them.
- **Every server GPU with Vulkan compute.** Fast path where 32-lane subgroups exist. A workgroup-memory path everywhere
  else (owner decision: small subgroups are supported). No server GPU is refused.

Non-goals: the client decode (part 3, separate spike); changing what the emitter, scheduler or wire do with a pass;
multi-threaded CPU encode.

## 2. Measurements this design rests on

Spike numbers. RX 480 / RADV, every tile CDF 5/3, synthetic noise / photo-like / text, GPU timestamp medians, reproduced
in two runs:

| per frame | 1080p (2040 tiles) | 4K (8160 tiles) |
|---|---|---|
| today: forward L1+L2+L3 | 12.55 ms (L1 10.7) | 45.8–48.7 ms (L1 42) |
| today: coefficient readback (i32) | 25 MB, 2.9–3.3 ms | 100 MB, 11.2–11.8 ms |
| today: CPU narrow + `encode_passes_sparse`, 1 thread | 145–399 ms | 570–1262 ms |
| parallel-lifting forward only | 0.21 ms | 0.80 ms |
| fused encode, subgroup RLE, 96 lanes | 1.11–1.22 ms | 4.27–4.52 ms |
| payload readback | 3.5–10 MB, 0.34–1.2 ms | 14–40 MB, 1.5–4.4 ms |

Inside the fused kernel at 4K:
- transform 0.8 ms;
- bit-planes and presence +0.66 ms;
- RLE about 2.2 ms with subgroup scans (3.5–5 ms serial);
- allocation 0.04 ms;
- copy-out 0.7–0.9 ms.

96 lanes per tile beat 192 lanes for two tiles everywhere: VGPRs went from 128 to 256 and occupancy halved. There were no
spills.

## 3. Delivery: two PRs

- **PR 1, parallel-lifting forward.** Replace `cdf53_forward_l{1,2,3}.comp` with one shader. Its output is exactly what
  L3 leaves today: i32 coefficients in the same buffer and layout. Readback, the CPU encode, `cdf53-diag` and every
  consumer stay unchanged. It removes about 12 ms (1080p) or 45 ms (4K) of GPU time per full-screen CDF 5/3 frame.
- **PR 2, fused encode.** The same shader gains the encode stages. The three entry points get payload buffers and
  records. The host turns records into `Cdf53Encoded`. The coefficient readback and the CPU encode go away.

PR 1 lands first and stands alone. PR 2 builds on its shader.

## 4. The shader

### 4.1 One source, generated lifting

`ghostframe-lib/src/capture/shaders/cdf53_encode.comp`, checked in with its `.spv` files like every other server shader.
The fully unrolled lifting functions (`lift_rows_N` / `lift_cols_N`, N = 32, 16, 8) are generated, as the spike does.
The generator is a Rust function in the test module. A unit test regenerates the source and fails if the checked-in
`.comp` differs, which keeps generated and committed code from drifting.

The `.spv` files are built by a new `just shaders-cdf53` recipe that runs `glslc` once per variant. Nothing is compiled
at build time. Today the server's `.spv` files are compiled by hand and nothing checks them. The recipe makes the
compile reproducible. No test compares `.spv` bytes, because glslc output differs between versions. What protects
correctness is the oracle gate (section 8): it runs the checked-in `.spv` itself, so a `.spv` that does not do what the
oracle does fails, stale or not.

Variants, chosen with `glslc -D` into separate `.spv` files:

| `.spv` | MODE | RLE | used by |
|---|---|---|---|
| `cdf53_forward.spv` | forward only, i32 coefficients | — | PR 1 production; PR 2 `cdf53-diag` |
| `cdf53_encode_sg.spv` | full encode | 32-lane subgroup scans | PR 2, devices passing 4.4 |
| `cdf53_encode_wg.spv` | full encode | workgroup-memory scans | PR 2, every other device |

### 4.2 Work shape

- One workgroup per tile, 96 invocations: 3 channels × 32. This is the same shape as the client's reconstruct shader.
- Input: the frame's B8G8R8A8 storage image, as L1 binds it today. Tile index from a list buffer, as L1 does today.
- **Forward.** Each invocation lifts one line of its channel in registers, i32, mirror boundary as `row_forward`.
  Level 1 has 32 lines of 32, level 2 16 of 16, level 3 8 of 8. One barrier separates a row pass from a column pass,
  never one barrier per element.
  - Work area in workgroup memory: 3 × 32 rows × 33 (padded stride) i32 = 12,672 B.
  - At the end the coefficients are in the per-channel layout `forward` produces (LL3, HL3, LH3, HH3, HL2 ... HH1).
- **Bit-planes.** Invocation (ch, w) builds word w of each of the 14 planes from layout coefficients 32w..32w+31. Plane 0
  is sign. Plane p = 1..13 is magnitude bit 13 − p of `unsigned_abs`. Bit i of a plane is bit i%32 of word i/32, which
  equals the wire's bit i%8 of byte i/8 on a little-endian word.
- **Presence.** `present_passes` is the OR over the workgroup of "plane p non-zero", with bit 0 always set.
- **RLE.** One stream per (present pass, channel), tokens exactly as `rle_encode`. Each 128-byte plane is one channel's
  32 words, so a stream's work maps to 32 invocations. Each invocation owns 4 plane bytes, computes how many output
  bytes they emit, and an exclusive prefix sum over the 32 gives its write offset. A zero byte that starts a run needs
  the next non-zero byte to know the run length: that is a suffix scan. The run cap of 128 never splits a run, because
  a plane is 128 bytes. The two variants differ only in how the scans are done (4.3).
- **Compaction.**
  - Stream lengths go through a workgroup prefix sum to give the tile's byte length. Pass 0 carries its 2-byte
    big-endian prefix, and each stream its 2-byte big-endian length.
  - One `atomicAdd` on the list's counter reserves the tile's space, 4-byte aligned.
  - The streams are written to the payload buffer, and the record is written last (4.5).

### 4.3 The two RLE variants

- **`_sg`:** the scans use `subgroupShuffleUp` / `subgroupShuffle` / `subgroupBallot` within aligned 32-lane groups,
  as in the spike.
- **`_wg`:** the same scans as a Hillis–Steele or Blelloch scan over the 32 entries in workgroup memory, with barriers.
  It needs basic compute only.

Lane identity in `_sg` is never `gl_LocalInvocationIndex`. Vulkan does not promise how invocations map onto subgroups
for a 96-wide workgroup, and `REQUIRE_FULL_SUBGROUPS` cannot be used because 96 is not a multiple of 64. Instead the
role index is `gl_SubgroupID * gl_SubgroupSize + gl_SubgroupInvocationID`, as Pyrowave's `dwt.comp` does. Every role
that needs 32-lane scans then sits in one aligned 32-lane cluster of one subgroup, for any subgroup size that is a
multiple of 32. A trailing partial subgroup (64 + 32 on wave64) has invocation ids 0..31 and is complete for its cluster.

### 4.4 Choosing the variant at start-up

`_sg` is used only when every one of these holds; otherwise `_wg`:
- the compute stage has the `BASIC`, `BALLOT`, `SHUFFLE` and `SHUFFLE_RELATIVE` subgroup operations;
- the subgroup size is a multiple of 32: either fixed so (`subgroupSize`), or pinned to 32 or 64 through
  `VK_EXT_subgroup_size_control` (`VkPipelineShaderStageRequiredSubgroupSizeCreateInfo`). The device is created at
  Vulkan 1.1, so the extension must be enabled when present;
- a **start-up self-test passes**: encode a fixed set of fixture tiles with `_sg` and compare with the CPU oracle. The
  fixtures are noise, a gradient, all-ones planes, `0x7F`/`0x80` patterns and an empty tile.

If the self-test fails, `_sg` is dropped and `_wg` is used, with one `warn` line naming the reason. `_wg` gets the same
self-test. If `_wg` also fails, there is no GPU CDF 5/3 encode on this device: one `error` line, and the tile path
treats every CDF 5/3 encode as failed, as it does today when the GPU forward fails. This is the same pattern as the
client's CDF 5/3 grid self-test. The chosen variant and the reason are logged once at `info`.

`GHOSTFRAME_CDF53_RLE=wg` forces `_wg`, read through `config.rs` per the env-read guard. It exists for A/B runs and so
that `_wg` can be exercised on hardware that would pick `_sg`. It is documented in AGENTS.md's probe list.

### 4.5 Outputs per list

Each of the three lists has its own set:

| buffer | memory | size | written by |
|---|---|---|---|
| payload | HOST_VISIBLE \| HOST_COHERENT, mapped | `capacity × CDF53_PASS_COUNT × MAX_PASS_PAYLOAD` (10,864 B/tile; 88.6 MB at 4K) | shader |
| records | HOST_VISIBLE \| HOST_COHERENT, mapped | `capacity × 48` | shader |
| counter | device-local, plus a host-visible copy | 4 B | shader `atomicAdd`; `vkCmdFillBuffer` to 0 before the dispatch |

Sizing per tile for the worst case removes any overflow path: no tile can make another fail. The memory is host
memory read over PCIe on a discrete GPU, which is how the coefficient buffers work today. The coefficient buffers this
replaces are 12,288 B/tile, more than the payload. PR 2 therefore needs no more host memory than today.

Record, 48 bytes (`uvec4` × 3), one per list slot:

```
word 0  offset (bytes, multiple of 4)
word 1  byte length of the tile's payload (all present passes, concatenated)
word 2  present_passes (low 16) | status (high 16: 0 = ok, non-zero = encode refused)
words 3..9  per-pass payload lengths, u16 x 14, pass 0 in the low half of word 3
words 10..11  reserved, 0
```

The payload for pass p starts at `offset + sum(len[q] for present q < p)`. A non-present pass has length 0. Lengths are
written for all 14 passes, so the host needs no table lookup.

## 5. The three entry points

| entry point | list | dispatch | today | PR 2 |
|---|---|---|---|---|
| dirty tiles | GPU compact list (`cdf53_compact`) | indirect, `cdf53_dispatch_args` | L1+L2+L3 → `cdf53_coefficients_buffer` → CPU encode | encode → dirty payload + records |
| escalation | host-staged escalation list | direct, `escalation_count` | L1+L2+L3 → `cdf53_escalation_coefficients_buffer` → CPU encode | encode → escalation payload + records |
| fallback | host-uploaded tiles, 512 per round trip (`forward_cdf53_chunk`) | direct | forward → coefficients → CPU encode (`frame_backend` and `emit.rs:510`) | encode → fallback payload + records |

- **PR 1** swaps the three-stage forward for one dispatch of `cdf53_forward.spv` in all three places. The L2/L3
  pipelines, descriptor sets, guards and barriers between them go away.
- **PR 2** dispatches `cdf53_encode_{sg,wg}.spv` instead. `forward_cdf53_tiles` becomes
  `encode_cdf53_tiles(&[&[u8]]) -> Result<Vec<Cdf53Encoded>, String>`, and `emit.rs` loses its CPU encode loop. The
  `#[cfg(test)]` CPU branch in `emit.rs` becomes `forward` + `encode_passes_sparse` on the CPU, as the oracle.

## 6. Host side (PR 2)

`frame_backend::finish` currently does, per tile: narrow i32 to i16, run `encode_passes_sparse`, push. It becomes, per
list slot:
1. Read the record. It is GPU-written memory, so check it like `clamp_gpu_tile_count`:
   - `offset % 4 == 0`;
   - `offset + len <= counter`, where `counter <= capacity bytes`;
   - the sum of the 14 lengths equals `len`;
   - a length is non-zero only where `present_passes` has the bit;
   - bit 0 is set;
   - `status == 0`.

   A record failing any check marks that tile's encode as failed, with a rate-limited `warn`. Failed tiles are retried
   by the commit, as a failed fallback forward is today.
2. Build `Cdf53Encoded { present_passes, passes }`, one `Vec<u8>` per present pass copied from the mapped payload. That
   is one copy, no parsing, and the same type and allocation pattern as today. The emitter, scheduler, `io_bridge` and
   their tests stay untouched.

Timings: `coef_read_ns` and `coef_encode_ns` are replaced by `payload_copy_ns`, which goes wherever the old ones were
logged. `cdf53-diag` still compares coefficients: with the feature on, PR 2 also dispatches `cdf53_forward.spv` into the
old coefficient buffer, and the diff runs as today.

Changing `Cdf53Encoded` to share one buffer (`bytes::Bytes` slices) would remove the per-pass copy. That is not part of
this work. The copy is bounded by the payload size: about 14–40 MB at 4K in the worst case, much less on a typical
damaged frame. Measure it in PR 2's report before considering it.

## 7. Errors and limits

- **Shader self-test failure:** as in 4.4. The tile path already handles "CDF 5/3 encode failed" per tile.
- **Bad record:** as in step 1 of section 6.
- **Counter beyond capacity:** cannot happen with per-tile worst-case sizing. The host check treats it as every
  unchecked record failing.
- **Device lost or submit failure:** unchanged from today's forward path.
- **Workgroup memory:** 12,672 B work area plus plane, length and offset scratch. The spike used 18,432 B per tile.
  Stays under the 32 KB `maxComputeSharedMemorySize` minimum required by Vulkan.

## 8. Testing

All GPU tests are `#[ignore]`d on CI hosts without a GPU, per the existing skip list, and are run locally through
`gf-gpu-run`.

- **Oracle gate, the main test.** For each variant (`forward`, `_sg`, `_wg`, with `_sg` forced only where 4.4 allows),
  over generated 1080p frames (noise, photo-like, text) plus fixed adversarial tiles:
  - the existing ceiling fixtures (all-ones planes, `0x7F`/`0x80..0xFF` bytes, full-range noise, max magnitudes);
  - an empty tile;
  - an edge tile.

  Every tile must be byte-equal to `forward` / `encode_passes_sparse`, with records consistent (section 6, step 1).
  The test fails on any mismatch.
- **Mutation tests.** The gate is proven to fail with the RLE escape threshold changed (as in the spike) and with the
  lifting rounding changed (`+2` → `+1` in the update step). Both must flag tiles.
- **Self-test.** A unit test runs the start-up self-test against a deliberately broken shader and asserts the fallback
  path and log reason.
- **Generated-source test**, from 4.1.
- **Host record validation**, CPU only: unit tests feed hand-built bad records (misaligned, overlapping, lengths not
  summing, length on an absent pass, status set) and assert the tile is failed and nothing panics.
- **Report.** The spike's `cdf53_spike_report` becomes `cdf53_encode_report`, `#[ignore]`d. It prints the PR's
  before/after numbers, and each PR body carries its table.
- **Before each push**, the full gate list from the handoff: the four `ci/check-*.sh`, fmt, clippy workspace
  all-targets plus the `cdf53-diag` clippy on every commit, lib tests with the GPU skip list, client-core tests, the
  cbindgen header, all browserless suites (release, `--test-threads=1`), and the Docker e2e with
  `GHOSTFRAME_E2E_REQUIRE_VAAPI=1` (`just containers-build` first).

## 9. What is not measured yet

To report in PR 2, not to block it:
- real desktop content (the spike used synthetic frames);
- latency in the live pipeline (`cumulative emit` and frame timings on a live session);
- the host's per-pass copy on a typical damaged frame;
- `_wg`'s cost against `_sg` on the RX 480.
