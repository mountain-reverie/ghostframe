# Native client M3: H.264 decode — design

**Status:** approved for planning
**Date:** 2026-09-23
**Predecessors:** [M1](2026-09-22-native-client-design.md), [M2](2026-09-23-native-client-m2-design.md)

M1 built the library, the GPU tile decoders and the dmabuf export. M2 made it
watchable: a tailnet to join, a window, input. Both negotiate
`supports_h264 = false`, so the server has never sent this client an H.264
frame. M3 decodes them.

---

## 1. Scope

**In:** VA-API H.264 decode of the full-frame access units `ClientCore` already
delivers; import of the decoded NV12 surface into wgpu; a native YUV→RGB blit;
a runtime capability probe that decides what the client advertises.

**Out, and why:**

- **The per-tile `Codec::H264` path.** `reassembly.rs` routes `Codec::H264`
  alongside `Codec::Skip` to "no per-tile RGBA path", on both clients. Frame
  mode is what the classifier actually reaches. Adding a second decode path for
  a codec neither client renders would be speculative work.
- **Software decode fallback** — decoding H.264 on the CPU with libavcodec when
  there is no hardware decoder. Decided in §6: such a machine advertises
  `supports_h264 = false` and runs on the tile codecs, which already work. The
  conditions that would justify building it are in §11. Not to be confused with
  §7's CPU *copy* fallback, which is about moving an already-hardware-decoded
  surface into wgpu when it cannot be imported directly.
- **The encoder's missing VUI signalling.** Real, pre-existing, and discussed in
  §5 — but it changes bytes on the wire for the shipped web client, so it is not
  M3's to fix.

---

## 2. Where this attaches

`ghostframe-client-core` already does the hard part. `frame_assembly.rs`
reassembles fragments and emits:

```rust
Event::NeedsH264 { frame_seq, timestamp_us, is_keyframe, payload }  // complete access unit
```

`Renderer::apply_event` currently matches it with an explicit no-op whose comment
says the reason is that M1 negotiates the capability off. That arm is the entire
integration seam. Nothing in the protocol, the transport, the reassembly layer or
the feedback path changes.

On the server, one bit does everything: `client_caps.rs` parses HELLO bit 1 into
`ClientCapabilities::supports_h264`, `io_bridge.rs` copies it into
`adaptation_context`, and `classifier.rs:560` stops overriding frame mode away
from H.264 once it is set.

---

## 3. Data flow

```
Event::NeedsH264 { payload }
  │
  ├─ H264Decoder::decode(payload, timestamp_us)      ffmpeg h264 + AV_HWDEVICE_TYPE_VAAPI
  │     └─ AVFrame(AV_PIX_FMT_VAAPI)
  │
  ├─ av_hwframe_map(→ AV_PIX_FMT_DRM_PRIME)          AVDRMFrameDescriptor:
  │                                                   objects[].fd, .format_modifier
  │                                                   layers[].planes[].offset, .pitch
  │
  ├─ import: the SAME dmabuf twice
  │     plane 0 → R8Unorm  at offset0, pitch0        (luma)
  │     plane 1 → Rg8Unorm at offset1, pitch1        (chroma, half resolution)
  │
  ├─ h264_nv12_blit.wgsl → framebuffer               full-range BT.601 inverse (§5)
  └─ ring.mark_dirty_all()                           H.264 replaces the whole frame
```

**Why `av_hwframe_map` and not `vaExportSurfaceHandle`.** ffmpeg is already a
dependency; libva is not. The DRM_PRIME descriptor carries the fds, per-plane
offsets and pitches, *and* the format modifier — everything the import needs —
without adding a second FFI surface with its own version pinning. The encoder
side of this repo already wraps the matching `AVBufferRef` lifecycles in
`ghostframe-lib/src/encoder/vaapi_device.rs`; M3 mirrors those patterns rather
than inventing new ones.

**Why import the same dmabuf twice.** Vulkan's route to NV12-as-one-texture is
`VK_KHR_sampler_ycbcr_conversion`, which wgpu's bind-group model cannot express.
Two single-plane views at the right offsets sidestep the problem entirely and
keep the conversion in WGSL where it can be tested. This was M1's plan (§7 of
that spec) and nothing has changed to undermine it.

**Threading.** Decode runs on the render thread, which already owns the GPU.
Hardware decode of a 1080p frame is single-digit milliseconds, but that is a
claim, not a measurement — §10 measures it, exactly as M2 measured the publish
stall instead of pre-emptively engineering around it.

---

## 4. Module layout

**New crate `ghostframe-client-h264`** — owns every unsafe ffmpeg call, knows
nothing about wgpu.

| File | Responsibility |
|---|---|
| `decoder.rs` | `H264Decoder`: codec context, VAAPI hw device ctx, send/receive, flush |
| `descriptor.rs` | `AVDRMFrameDescriptor` → `DmabufPlanes` (a plain Rust struct: fd, modifier, per-plane offset/pitch/size) |
| `probe.rs` | Can this machine decode H.264 through VA-API? |

**`ghostframe-client-gpu` gains:**

| File | Responsibility |
|---|---|
| `import.rs` | `DmabufPlanes` → `wgpu::Texture` pair. The mirror of M1's `export.rs` |
| `pipelines/h264_nv12.rs` | Bind groups, sampler, render pipeline for the blit |
| `shaders/client/h264_nv12_blit.wgsl` | The conversion itself |

`client-gpu` depends on `DmabufPlanes`, not on ffmpeg. That boundary is what
makes §9's two oracles possible: the pipeline can be driven with synthetic NV12
and no decoder, and the decoder can be tested with no GPU surface. It also keeps
ffmpeg out of the crate that every tile codec lives in.

`ghostframe-client-native` wires the two together and owns the probe result.

---

## 5. Colour: full-range BT.601, not BT.709

**The M1 spec was wrong about this and the shader must not repeat it.** It called
for "BT.709 limited-range YUV to RGB". The server does not encode that.
`ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp` says so in its own
comment — `// BT.601 full range` — and computes:

```glsl
float Y_f = 0.299 * R + 0.587 * G + 0.114 * B;          // no 16-235 scaling
float U_f = -0.169 * R - 0.331 * G + 0.500 * B + 0.502;
float V_f =  0.500 * R - 0.419 * G - 0.081 * B + 0.502;
```

The `0.502` offset is deliberate and correct: it centres chroma on 128, not on
127.5, which is where 8-bit neutral actually sits.

`h264_nv12_blit.wgsl` uses the **exact inverse of that matrix**:

```wgsl
let y = luma;
let u = chroma.r - 0.502;
let v = chroma.g - 0.502;
let r = y - 0.000927 * u + 1.401687 * v;
let g = y - 0.343695 * u - 0.714169 * v;
let b = y + 1.772160 * u + 0.000990 * v;
```

These differ from the textbook full-range BT.601 inverse (`1.402 / -0.344136 /
-0.714136 / 1.772`) by at most **0.171 of a 255-step across the entire YUV cube**
— computed over the cube, not assumed — so the choice is not load-bearing for
output quality. It is made because inverting the transform the encoder actually
applied is the honest thing to do, and because it costs nothing.

**Chroma upsampling is nearest-neighbour, not bilinear.** The forward shader
takes chroma from the top-left pixel of each 2×2 block (`if (local_idx == 0)`)
rather than averaging. Replicating that sample across the block inverts it;
bilinear would smooth toward something the source never contained. The shader
header must say this, because "upgrading" it to bilinear is an obvious-looking
improvement that silently diverges from the source.

### 5.1 What this means for the browser

The encoder writes no VUI colour description — there is none in `h264_vaapi.rs`
or `nal_parser.rs` — so the stream tells decoders nothing and each applies its
own default. Chrome's WebCodecs will typically assume BT.709 limited range for HD
content. If that holds, the web client has been rendering systematically shifted
colour since H.264 mode shipped, and **the native client will be the more correct
of the two.**

Two consequences, both deliberate:

1. The M1 spec's proposed gate — decode the same clip natively and in the browser
   and compare — **would fail by design.** It is not part of M3's acceptance.
2. This is recorded as a suspected encoder bug for a later milestone, stated as a
   suspicion: I verified the encoder side, not Chrome's assumption. Confirming it
   means measuring what Chrome actually produces, which §11 records as the way to
   settle it.

---

## 6. Capability probe

`gf_client_create` probes **before** `ClientCore` is constructed. HELLO's
capability byte is built at connect and never revised, so a probe that ran later
could not affect it.

The probe opens a VA-API device on `/dev/dri/renderD128`, confirms an H.264
decode entrypoint (`VAEntrypointVLD`), and tears down. Failure is not an error:
it logs at `info` and leaves the bit clear, and the session runs on the tile
codecs exactly as every session does today. The failure mode is "somewhat less
efficient", never "broken".

`gf_client_config.supports_h264` changes meaning from *assertion* to
*permission*: the effective value is `config.supports_h264 && probe_succeeded`.
A host that passes `true` on a machine without VA-API gets a working session, not
a failure. A new `gf_client_supports_h264()` getter reports the effective value,
since a host that asked for H.264 has a legitimate interest in whether it got it.
`ghostframe-cli` stops hardcoding `false` and passes `true`.

### 6.1 Flipping the bit changes first paint

`io_bridge.rs:3435` sets `frame_mode = FrameMode::H264` on session reset, so a
session *begins* in H.264 mode. The moment this client advertises the capability,
its **first frames are H.264**, not tile codecs.

That is the single most consequential line in this design. It means a broken
decode path shows up as a black window at startup rather than as a subtle
degradation under motion — which is good for catching it, and bad for anyone who
flips the bit before §7's import question is settled. It also means the two
existing acceptance tests need thought: `native_client.rs` passes
`supports_h264: false` explicitly and is unaffected, but `showcase.rs` builds its
config through the CLI path, so flipping the CLI default changes what that test
renders. It pins `GHOSTFRAME_TEST_FORCE_FRAME_MODE=tile` to stay a test of the
tile path, or it stops being the test it was written to be.

**The probe must not be trusted further than it tests.** It answers "can VA-API
decode H.264 here", not "can the decoded surface be imported" — §7's open
question. If the spike shows import can fail on otherwise-capable hardware, the
probe grows to cover it; that is a Task 1 output, not an assumption to bake in
now.

---

## 7. Import, and the open question

The decoded surface arrives as a dmabuf with a DRM format modifier. Importing a
**tiled** modifier into Vulkan requires `VK_EXT_image_drm_format_modifier`.

**This RX 480 / RADV Polaris10 does not expose that extension** — confirmed
against `vulkaninfo`, and the same gap M1 hit. What it does expose is
`VK_EXT_external_memory_dma_buf` and `VK_KHR_external_memory_fd`, which import a
**linear** buffer through `VK_IMAGE_TILING_LINEAR` — the mirror of the path M1's
`export.rs` already uses successfully.

So everything turns on one unknown: **does radeonsi export decode surfaces
linear or tiled?** Polaris-era UVD commonly produces tiled surfaces, but Mesa's
behaviour on export is precisely the kind of detail that costs a day when
assumed. **Task 1 is a spike that answers it** — decode one frame, map to
DRM_PRIME, print `format_modifier` and the plane layout — before any code depends
on the answer.

The three outcomes, all of which the design survives:

| Spike result | Path | Cost |
|---|---|---|
| Linear | Import directly | Zero-copy |
| Tiled, VPP-to-linear works | VA-API VPP pass into a linear surface, then import | One extra GPU pass |
| Tiled, VPP unavailable | `av_hwframe_transfer_data` into a CPU NV12 frame, `write_texture` | ~3 MB/frame over the bus |

`VAEntrypointVideoProc` is present on this card, so the middle row is plausible —
but forcing a *linear* VPP target may need surface attributes that ffmpeg's
filter API does not expose, which would drop us to the third row on this
hardware. The CPU fallback is one ffmpeg call, so the milestone cannot be blocked
outright, only made slower on this GPU. There is precedent: the VKMS cross-GPU
e2e path already falls back to CPU mmap for the same class of reason.

Whichever path runs, it logs which one at startup. A silent fallback is how a
performance regression hides for a month.

---

## 8. Error handling

**Decoded size ≠ framebuffer size** → drop the frame, log at `warn`. The tile
stream's `FrameDimensions` and the H.264 SPS disagreeing means something is
wrong upstream; blitting anyway corrupts the framebuffer, and the next keyframe
recovers. Dropping is recoverable, corruption is not.

**Decode error** → log at `warn` and wait for the next keyframe.

This is weaker than it first looks, and the weakness is worth stating. The
`DecodeError` feedback path runs in `ClientCore`, which reports *tile* decode
failures it detects itself; the renderer sits downstream of it and has no
channel back. So an H.264 decode failure is visible locally and invisible to the
server, which will keep sending H.264 to a client that cannot decode it. M1's
existing `DecodeError` arm has the same shape and the same limitation.

Accepted for M3 because the recovery does not depend on the server knowing: the
next keyframe fixes the stream, and a client whose hardware cannot decode at all
never advertises the capability in the first place (§6). Wiring renderer-side
failures into the feedback stream is a real improvement, and belongs with a
decision about what the server should *do* with it — which is more than this
milestone should decide on its own.

**No keyframe yet** → ffmpeg returns `EAGAIN` until it has one. Feed packets,
emit nothing, do not log per frame. This is normal at session start, not an
error.

**Resize during H.264 mode** → `Event::FrameDimensions` already flushes and
resizes the framebuffer and the export ring. The decoder is *not* reset: H.264
carries its own SPS, and a stream that genuinely changes resolution sends a new
one. The size check above catches any disagreement.

---

## 9. Testing

Two exact oracles and one toleranced acceptance test. The exactness is not
aspirational — it follows from what each layer guarantees.

**`oracle_h264_decode` — hardware vs software decode, bit-exact on NV12.**
H.264's inverse transform is specified exactly by the standard; conforming
decoders produce identical output for identical bitstreams. So VA-API output and
libavcodec's software decoder must match byte for byte, and any drift is a real
bug in how we drive the hardware, not codec noise. No tolerance.

**`oracle_nv12_blit` — WGSL vs a Rust reference, on synthetic planes.** Same
constants, same nearest-neighbour chroma, asserted exactly. Coverage must include
the full-range edges (Y=0 and Y=255), where a limited-range matrix produces
visibly different output — so if someone later "corrects" the shader to BT.709
limited, this test fails loudly instead of shifting every colour by a little.

If GPU and CPU f32 diverge in the last bit at a rounding boundary, the response
is to measure how often and why, and record it — **not** to open a tolerance
pre-emptively. M1's CDF 5/3 episode is the precedent: a tolerance written on an
unverified assumption was wide enough to hide the systematic drift the oracle
existed to catch.

**`e2e_h264` — live server, H.264 frame mode, toleranced.**
`GHOSTFRAME_TEST_FORCE_FRAME_MODE=h264` pins the classifier, so the test does not
depend on the adaptation policy choosing H.264 on its own. Session entry already
starts in H.264 (§6.1), so the first frames would arrive H.264-encoded anyway —
but "anyway" is how a test becomes load-bearing on a default nobody meant to
depend on. Pin it. The tolerance covers
encoder quantisation, which is genuinely lossy and which no assertion can undo;
the number goes in with the measured error beside it, not as a round figure
chosen to make the test pass.

**Import unit test** — a known dmabuf imported and read back, so an import bug
is distinguishable from a decode bug.

**CI:** the VA-API and GPU tests are not named in any workflow, matching
`native_client` and `showcase`. Runners have neither a GPU nor VA-API. Per the
"CI exempts itself" rule the skip lives in the workflow's silence, never in an
`#[ignore]` that would hide the test on developer machines too. `ghostframe-cli`
and the pure-logic parts of the new crates *are* CI-visible and must be named
explicitly — a new `tests/*.rs` is invisible until a workflow lists it.

**Mutation check:** mutate out the R↔B channel assignment and confirm
`oracle_nv12_blit` fails. That specific bug shipped once already (fixed in
`847d870`, on the encoder side) and cost real debugging time.

---

## 10. Measurement

M2's precedent: measure, then decide, and write the numbers down.

- Decode time per frame (submit → surface available), p50/p99.
- Import path taken (direct / VPP / CPU) and, if CPU, the copy cost.
- Whether decode on the render thread perturbs the publish cadence measured in
  M2 (p50 97µs, p99 624µs steady-state).

If decode blocks the render thread enough to matter, a decode thread is the
answer — but that is a conclusion to reach from a number, not from the shape of
the code.

---

## 11. Deferred, with the conditions

**Software decode fallback.** Build it when a target machine is found that has no
VA-API H.264 decode *and* needs H.264 — e.g. an NVIDIA proprietary setup without
the VA-API bridge. Until then, tile codecs cover that machine.

**The VUI colour signalling bug (§5.1).** Settle it by decoding a known clip in
Chrome and comparing against the native output; the size of the difference says
whether Chrome assumes BT.709 limited. If confirmed, the fix is in the encoder —
set `colorspace`/`color_range` on the `AVCodecContext` so the stream says what it
is — and it changes rendering for the shipped web client, which needs its own
milestone.

**Fence export.** Still deferred from M1 and re-measured in M2 (p99 624µs against
a 16.67ms budget). M3 re-checks it only if §10 shows decode changed the picture.

---

## 12. Task shape

Roughly thirteen: the modifier spike; decoder; descriptor; probe; import;
shader; pipeline; renderer wiring; the two oracles; e2e; the CAPI/CLI capability
flip; measurement.

Task 1 is the spike, and it comes first because everything in §7 branches on its
answer.

---

## 13. Risks

1. **Tiled export with no route to linear** (§7). Mitigated by the CPU fallback,
   which is one ffmpeg call. Slower on this GPU, never blocked.
2. **f32 divergence between the WGSL and the Rust reference** (§9). Mitigated by
   measuring before tolerating anything.
3. **ffmpeg version drift.** `ffmpeg-next` is pinned `=9.0.0` and must match the
   host libavcodec; the hardware APIs need `ffmpeg_sys_next` and raw `unsafe`,
   and the `Send` impls have to be written by hand. All three are already true
   of the encoder side, so the patterns exist to copy.
4. **A new workspace member breaks the e2e container image.** The test-server
   Dockerfile copies manifests per crate; `ghostframe-client-h264` must be added
   or `docker buildx` fails before any test runs.
5. **New system dependency in CI.** The new crate links libavcodec, which CI
   already installs. Confirm rather than assume — M2 went red on exactly this
   class of gap, where the dev box had a library the runner did not.
