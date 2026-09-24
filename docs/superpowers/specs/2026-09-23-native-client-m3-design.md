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

### 7.1 Spike result (Task 1, 2026-09-23)

Ran the Task 1 spike on this machine (RX 480, RADV Polaris10, Mesa 26.1.7
radeonsi, `libva` driver `Mesa Gallium driver ... radeonsi, polaris10, ACO`).
`vainfo` confirms `VAProfileH264Main`/`VAProfileH264High` at `VAEntrypointVLD`
before anything else was attempted, so the decode path itself is healthy.

The spike compiled and ran **unmodified** — no fixes needed to the code in the
plan. `cargo run --release` decoded one frame (of 3 encoded access units; the
decoder wasn't flushed with EOF before the loop exited, so only the first
output frame was observed — irrelevant to this question, since every plane
descriptor is stable across 3 separate runs). Output:

```
[spike] frame 640x480 format=AV_PIX_FMT_VAAPI
[spike] nb_objects=1 nb_layers=2
[spike]   object[0] fd=6 size=552960 modifier=0x00ffffffffffffff
[spike]   layer[0] format=0x20203852 nb_planes=1
[spike]     plane[0] object_index=0 offset=0 pitch=768
[spike]   layer[1] format=0x38385247 nb_planes=1
[spike]     plane[0] object_index=0 offset=368640 pitch=768
```

**The modifier is `0x00ffffffffffffff` — `DRM_FORMAT_MOD_INVALID`, not
`DRM_FORMAT_MOD_LINEAR` (0).**

**And on this chip that value carries no information whatsoever.** AMD format
modifiers begin at GFX9: the lowest tile version in `/usr/include/drm/drm_fourcc.h`
is `AMD_FMT_MOD_TILE_VER_GFX9 1`, and radeonsi's `ac_is_modifier_supported()`
returns false for `gfx_level < GFX9` *before* it would accept even
`DRM_FORMAT_MOD_LINEAR`. Polaris10 is GFX8. So `INVALID` is the only value
`vaExportSurfaceHandle` can return here — for a linear surface and a tiled one
alike. Reading it as evidence of tiling is reading a field that has no
vocabulary to say anything with.

The mapping did take the modern export path, not a legacy fallback. ffmpeg's
`vaapi_map_to_drm_abh()` picks the first `vaapi_drm_format_map` entry matching
the VA fourcc, and that table lists `DRM_FORMAT_RG88` (`0x38384752`) ahead of
the `GR88` variant; the spike observed `GR88` (`0x38385247`), which is what
Mesa passes straight through under `vaExportSurfaceHandle(PRIME_2)`. The two
fourcc values were checked here; the table ordering is from a review of ffmpeg
n9.0.1's `libavutil/hwcontext_vaapi.c` and has not been re-verified locally
(the source is not installed on this machine).

Shape observed: **one object, two layers, one plane each** — `object[0]` is a
single dmabuf (fd, 552960 bytes) backing both layers. `layer[0]` format
`0x20203852` decodes (little-endian fourcc bytes) to `"R8  "` (`DRM_FORMAT_R8`,
the Y plane); `layer[1]` format `0x38385247` decodes to `"GR88"`
(`DRM_FORMAT_GR88`, the interleaved UV plane) — i.e. this is exactly the NV12
shape M1/M3 assumed, not the "one layer with two planes" alternative. For
640x480: Y at offset 0, pitch 768 (640 padded up to a 256-byte multiple);
UV at offset 368640 (= 768 × 480, i.e. right after the full padded Y plane),
pitch 768, height 240. `768 × 480 + 768 × 240 = 552960`, which matches
`object[0].size` exactly.

**What this does and does not establish.** The plane arithmetic is
self-consistent, which rules out 2D/macro-tiling (that pads height and the
slice base, changing the total size). It does **not** distinguish linear from
GFX8 1D/micro-tiling: 8×8 micro-tiling reorders bytes *within the same
allocation footprint*, so pitch, offset and size are identical either way. The
one piece of evidence available is blind to the most likely alternative.

**Linearity was unmeasured here, not disproven — §7.2 has since measured it,
and the answer is linear.** Read that section before acting on this one. The
rest of this paragraph is kept because the reasoning still holds: the absence
of `VK_EXT_image_drm_format_modifier` never ruled direct import out.

**Direct import is not ruled out.** The absence of `VK_EXT_image_drm_format_modifier` rules out importing a
*tiled* modifier; it says nothing about the linear path, which
`ghostframe-client-gpu/src/export.rs` uses in production on this exact GPU
today, precisely because the extension is missing. An earlier draft of this
section called that extension an independent second reason to abandon direct
import. It is not a reason at all.

Two things settle it. The first has now run (§7.2); the second ships in Task 6:

1. **Task 5's linearity check** (added after this review): `mmap` the exported
   dmabuf and compare it byte for byte against `av_hwframe_transfer_data`'s
   output, which is authoritative. Equal means the buffer *is* linear at the
   descriptor's exact layout, whatever the metadata declines to say. The
   `mmap` + `DMA_BUF_IOCTL_SYNC` wrapper already exists in `export.rs`, and the
   oracle already calls `av_hwframe_transfer_data` on the same surface.
2. **Task 6's runtime pitch check**, which was in the plan before any of this:
   create the image, ask `vkGetImageSubresourceLayout` what pitch the driver
   actually chose, and refuse the import if it disagrees with the descriptor.
   That is what makes attempting an un-promised linear import *safe* — it
   either verifies at runtime or fails cleanly, instead of rendering a sheared
   frame.

Task 6 gets built, and the import path is chosen at runtime with the startup
log §7 already requires. The CPU copy (`av_hwframe_transfer_data` into a CPU
NV12 frame, then `write_texture`, built across Tasks 8 and 9) remains the
guaranteed floor and is needed regardless — it is the portability fallback for
any machine whose surface cannot be imported.

**A note on how this section went wrong first.** The plan instructed the spike:
"if the modifier is not 0, state that direct import cannot work on this GPU."
That rule silently assumed `modifier != 0` implies tiled. On GFX8 the modifier
is *structurally* never 0, so the rule fired on a hardware fact that has
nothing to do with tiling, and the spike faithfully recorded a conclusion the
evidence never supported. The defect was in the plan, not the spike. A spike
step that pre-commits to a conclusion for an unseen measurement deserves the
same suspicion next time.

### 7.2 Linearity measurement (Task 5, 2026-09-23)

§7.1 left linearity **unmeasured**: the modifier is structurally `INVALID` on
this GFX8 chip and the plane arithmetic (pitch/offset/size) cannot distinguish
a linear layout from GFX8 1D/micro-tiling, which reorders bytes within the
same footprint. Task 5 settles it without any GPU API, per the plan
(`ghostframe-client-h264/src/oracle_tests.rs::
the_exported_dmabuf_is_linear_at_the_descriptors_layout`): `mmap` the exported
dmabuf read-only at the descriptor's own offsets and pitches, and compare it
byte for byte against `av_hwframe_transfer_data`'s output on the same
`HwFrame` -- the same call this crate already trusts as ground truth in the
decode oracle next to it.

Measured on the same machine as §7.1 (RX 480, RADV Polaris10, Mesa 26.1.7
radeonsi), at two resolutions, comparing **one decoded frame** at each:

```
[m3] dmabuf-vs-download 640x480:   0 of 460800 bytes differ (luma 0, chroma 0)
[m3] dmabuf-vs-download 1920x1080: 0 of 3110400 bytes differ (luma 0, chroma 0)
```

`460800 = 640*480*1.5` and `3110400 = 1920*1080*1.5`, i.e. the full luma plane
and the full interleaved chroma plane in both cases. The mmap reads at the
descriptor's own offsets and pitches (768 for a 640-wide frame), so the
comparison exercises the stride handling as well as the content.

**1920x1080 is measured, not extrapolated, and that is the point.** radeonsi
chooses tiling per surface from its dimensions and alignment, so a result at
640x480 says nothing about the resolution this client actually runs at. An
earlier draft of this section generalised from the small clip alone; the
second measurement exists because that generalisation was not safe to make.

**Both planes are load-bearing.** The test can only detect tiling if the
content varies within a tile, and `gradient_clip`'s chroma pattern originally
did not vary down `y` — meaning a layout that permuted chroma *rows* would have
produced zero diffs there, leaving luma to carry the whole finding. The pattern
now varies in both axes on both planes. Re-measured after that change: still
zero diffs, so the result was not an artefact of the weaker pattern.

**Reading:** at 640x480 and at 1920x1080 on this Polaris10 part, the surface
radeonsi exports for H.264 decode **is linear at the descriptor's exact
layout**, despite the modifier field being structurally incapable of saying so
(§7.1). `import_nv12`'s `VK_IMAGE_TILING_LINEAR` path (§7, row 1) is therefore
expected to work here.

**What this does not establish.** One frame at each of two sizes, one driver,
one GPU generation. It does not prove linearity at every resolution the server
might negotiate, on another Mesa revision, or on other hardware. It does not
need to: Task 6's runtime `vkGetImageSubresourceLayout` pitch check compares
what the driver actually chose against what the descriptor claims and refuses
the import on disagreement, and the CPU copy stays in the build as the floor.
This measurement upgrades direct import from "unfounded assumption" to
"expected, and verified at runtime before it is trusted" — not to "guaranteed".

**This is now a gate, not just a record.** The test asserts zero diffs
whenever the modifier is `LINEAR` or `INVALID` — the two cases where a linear
layout is what we are relying on — and prints-and-skips on a real tiled
modifier, since comparing a legitimately tiled buffer against a linear read is
expected to differ and says nothing about a bug. So a driver revision that
changed the tiling here fails this test with the byte counts in the message,
rather than being discovered later as an opaque import failure. Both
resolutions are separate tests, so a size-dependent change names the size.

This closes the open question §7 posed. It does not change anything about
*why* the modifier is uninformative on GFX8 (§7.1 already explains that
correctly) -- it answers the question §7.1 said the modifier could not.

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

### 9.1 That measurement, taken — and then taken again, on real hardware

The first attempt measured the CPU reference against a *model* of the GPU and
reported 224 divergences: 170 from a double-rounding defect in the reference,
54 attributed to FMA contraction. The first number was right and that defect is
fixed. **The second was model-vs-model noise, and the real divergence is four
orders of magnitude larger.**

Two independently built harnesses — one feeding real NV12 textures, one feeding
CPU-computed constants through a storage buffer with no textures at all —
rendered the shipped shader to a real `Rgba8Unorm` target on this RX 480 and
landed on the same count:

```
total channel-samples : 50,331,648
GPU != CPU reference  :  1,420,203  (2.8217%)
worst |GPU - CPU|     :  1 LSB, always GPU = CPU - 1, never +1
```

**The mechanism, confirmed at the instruction level.** Same shader, same
arithmetic, only the render-target format changed:

```
Rgba8Unorm:   v_cvt_pkrtz_f16_f32 v3, v3, v4   ; pack to f16, Round Toward Zero
              exp mrt0 ... compr vm            ; COMPRESSED (fp16) export
Rgba32Float:  exp mrt0 ...        vm           ; no packing
```

On GCN/Polaris an 8-bit UNORM colour target uses the `SPI_SHADER_FP16_ABGR`
export format, so ACO is *required* to pack fragment outputs to fp16 first, and
that pack rounds toward zero. The ROP then converts fp16 → unorm8 with correct
round-half-away:

```
gpu_byte(v) = round_half_away( f16_round_toward_zero(v) * 255 )
```

Zero mismatches against that model over the full cube. f16 ulp on `[0.5, 1)` is
2⁻¹¹ = 0.1245 in units of 1/255, so truncation can only flip a rounding when the
exact fractional part lands in `[0.50, 0.6245)`, and within that window it
depends on how far below the next f16 the value sits. An earlier empirical
sketch — "floor when the fraction is in [0.51, 0.61]" — fit the bulk and
mispredicted individual samples.

Ruled out by measurement rather than argument: dithering (each constant rendered
at 8 y-positions, 0/63 position-dependent), sRGB, blending, MSAA, plain
truncation, and round-to-even.

**FMA contraction does not occur here.** ACO folds each multiply-add, but into
`v_mad_f32` — the *non-fused* GFX8 MAD, which rounds the product before the add.
A float-target comparison over 16,908,288 channel values found **0 bit
differences** from non-contracted Rust f32. The premise (naga emits no
`NoContraction`, so a driver *may* fuse) was right; the conclusion was not.
GFX10+ dropped `v_mad_f32` for `v_fma_f32`, so this could become observable on
another AMD generation — an argument for keeping the float tier below, not for
pre-emptively tolerating anything.

### 9.2 What the oracle asserts, and why not a tolerance

**Tier A — render to `Rgba32Float`, assert f32 bit-equality** against the CPU
reference's pre-quantisation channel values. Achievable exactly: 0 differences
in 16,908,288 comparisons. This is the arithmetic gate, at full precision.

**Tier B — render to the real `Rgba8Unorm` framebuffer, assert equality against
`round_half_away(f16_rtz(v) * 255)`.** Also exact: 0 mismatches in 50,331,648.
That quantiser lives **in the test**, labelled as a model of the AMD
compressed-export path — not in `nv12_reference.rs`, which models a conformant
write. On non-AMD hardware, relax only Tier B to the two-element set
`{round(v), round(f16_rtz(v))}` — adjacent by construction, never a magnitude
tolerance — and let Tier A carry arithmetic correctness. Tier B's remaining job
is the plumbing: chroma `p/2` indexing, plane strides, orientation, all of which
fail by enormous margins.

**Why a blanket 1-LSB tolerance was rejected**, measured against the shipped
matrix over the full cube:

| substitution | differing | > 1 LSB | max |
|---|---|---|---|
| textbook BT.601 full-range inverse | 3.216% | **0.000%** | **1** |
| BT.709 full-range inverse | 66.427% | 60.713% | 52 |
| BT.601 **limited** range | 64.754% | 60.374% | 21 |
| cb/cr swapped (indexing bug) | 86.181% | 85.318% | 255 |

BT.709, limited range and chroma swaps are caught by anything. But the
"helpful correction" to textbook BT.601 constants — the exact edit the shader
header spends a paragraph warning against — **never exceeds 1 LSB**. A 1-LSB
tolerance is precisely blind to the only mutation subtle enough to need a gate.
An exact tier flags it on 1.6 million samples.

### 9.3 Two consequences worth recording

**This is in shipped output, not just tests.** `framebuffer.rs` is
`Rgba8Unorm`, so every production H.264 blit carries this ≤1 LSB,
always-downward bias. Invisible, not worth fixing — but recorded here as a
characterised hardware property rather than resurfacing later as an unexplained
discrepancy.

**The other codecs are unaffected.** Solid, PalRle and Cdf53 write exact `k/255`
values, and all 256 were verified to round-trip through f16-RTZ → unorm8 back to
`k`. Only the H.264 blit produces the arbitrary intermediates that can land in
the `[0.50, 0.6245)` window.


## 10. Measurement

M2's precedent: measure, then decide, and write the numbers down.

**Revision note (2026-09-24):** the first version of this section timed only
`decoder.decode()` and concluded "no decode thread justified" at a claimed
22% of budget. A review round reproduced the measurement independently and
found the timed span excluded the hardware decode itself — ffmpeg's VA-API
hwaccel does not synchronise the decoded surface inside
`avcodec_receive_frame`; the `vaSyncSurface` wait lives in the map-to-DRM
path, i.e. inside `HwFrame::map_dmabuf`, called later in `blit_h264_frame`,
after the original timer had already stopped. That review also found
`H264Decoder::new()`'s one-time cost excluded from the "one-time stall"
number, and the test using a non-production memory type
(`host_visible: true`) for the publish-cadence comparison. All three are
fixed below: `map_dmabuf` and `H264Decoder::new()` are now timed
(`ghostframe-client-gpu/src/renderer.rs`), the harness passes
`host_visible: false` to match production
(`ghostframe-client-gpu/tests/gpu_h264_render.rs`), and both an unpaced and a
60Hz-paced run were captured, because the review's own reproduction showed
pacing changes the `map_dmabuf` distribution substantially. §10.2-§10.6 below
are the corrected numbers and verdict; nothing here should be read as
correcting §10.3 (import path) or the parts of §10.5 unaffected by the sync
gap, which held up.

### 10.1 How this was collected

`ghostframe-e2e/tests/h264.rs` (added after the first draft of this section,
in a later commit) is the real e2e acceptance test — a live server, a real
tailnet, GPU capture on the server side — but it installs no tracing
subscriber (see its own module doc for why) and so has no debug-log output
to measure from. This section still uses
`ghostframe-client-gpu/tests/gpu_h264_render.rs`'s two `#[ignore]`d
`decode_and_publish_timing_at_1920x1080_{unpaced,paced_60hz}` harnesses
instead. They feed real decoded access units through `Renderer::apply_event`
exactly as the render thread does (`render_thread.rs`'s per-event `decode ->
flush -> publish -> release`), on this machine's real VA-API hardware (RX
480, RADV Polaris10 — the same GPU as §7.1/§7.2, and the same GPU `h264.rs`
runs against). What neither harness does is talk to a live server: the
access units come from `gradient_clip`, a synthetic moving-gradient clip
encoded once with libx264 at default settings. That means the bitrate and
keyframe cadence are whatever libx264's defaults produce for a gradient, not
production desktop content or the server's actual `h264_vaapi.rs` encoder
settings — real content (text, static regions, scene cuts) compresses
differently and could shift the per-frame byte count `avcodec_send_packet`
copies. One run of each variant, one GPU, one clip; not a swept distribution.

Three spans are now timed with `std::time::Instant`, all in
`ghostframe-client-gpu/src/renderer.rs`, all logged at `debug` on the
`ghostframe_client_gpu::renderer` target, same precedent as `ring.rs`'s
existing `publish` timing (`#[allow(clippy::disallowed_methods, ...)]`, off
by default):

- `new_us` — `H264Decoder::new()`, the one-time session-startup device open.
- `decode_us` — `decoder.decode(au)`: submit through
  `avcodec_receive_frame` returning. **Not** "submit to surface available" —
  see the revision note above and `decode_h264`'s doc comment at that call
  site for why.
- `map_us` — `frame.map_dmabuf()` in `blit_h264_frame`: this is where
  `vaSyncSurface` actually waits for the hardware decode to finish.

`ring::publish`'s existing `poll_us` is the fourth number, unchanged. Both
runs below are 120 frames at 1920x1080 (the production resolution), captured
with
`RUST_LOG=ghostframe_client_gpu::renderer=debug,ghostframe_client_gpu::ring=debug`.
"Steady" below excludes the first 5 frames (frame 0's one-time cost plus
frames 1-4's B-frame-reorder pipeline fill, per §10.2).

### 10.2 The one-time session-startup cost: ~43-46 ms, not 32.66 ms

```
                          UNPACED    PACED (60Hz)
H264Decoder::new()        12.75 ms   15.13 ms
frame 0 decode_us         30.75 ms   30.82 ms
                          --------   --------
first-frame total         43.49 ms   45.95 ms
```

The first draft of this section attributed frame 0's cost to "VA-API
device/surface-pool setup happening lazily inside the first
`avcodec_send_packet`" and reported only that number (32.66 ms in that run).
That was wrong in two ways: device setup (`av_hwdevice_ctx_create`) happens
inside `H264Decoder::new()`, called *before* `decode_h264`'s timer starts —
excluded from every §10.2 number in the first draft, not the source of frame
0's cost. What frame 0's ~30.7-30.8 ms actually contains, matching this
project's own `decoder.rs`, is SPS parse → `get_format` → hw-frames-context
and surface-pool init → `vaCreateConfig`/`vaCreateContext` → the first
submission — the sentence that matters if something here is to be
pre-warmed, since it names what the warm-up call needs to trigger, not just
that a stall exists.

`new_us` varied between the two runs (12.75 ms vs 15.13 ms) despite running
back to back in the same process invocation via `cargo test`, each in its
own `#[ignore]`d test — real driver/kernel-state variance, not a
measurement artifact; treat both as one-run samples, not a stable constant.
**Total first-H.264-frame stall is ~43.5-46 ms, roughly 2.6-2.8 sixty-hertz
frame budgets** — this is the number that should drive any "warm the VA-API
context at startup" decision, not the ~32.7 ms the first draft credited it
with. It remains a one-time, per-session cost (open once, decode 10 frames
or 10,000, same startup bill), not a per-frame one, and warming the context
before the first real access unit arrives is still a smaller, more targeted
fix than a decode thread.

### 10.3 Import path

**Zero-copy dmabuf import ran for all 120 frames** in both runs —
`blit_h264_frame`'s `H.264 import path: zero-copy dmabuf` logged once and no
CPU-copy fallback path fired, confirming Task 9's finding (§7.2) holds under
a longer run. There is no CPU copy cost to report on this hardware. (This
finding was not affected by the sync-timing gap and is unchanged from the
first draft.)

### 10.4 Steady-state per-frame cost: decode, the sync wait, and publish

```
                        UNPACED (n=115/117)          PACED 60Hz (n=115/117)
decode_us   min=106  p50=122  p90=142  p99=190  max=323    min=106  p50=156  p90=233  p99=306  max=345
map_us      min=3    p50=758  p90=3035 p99=3854 max=3914   min=3    p50=9    p90=2898 p99=4034 max=4071
poll_us     min=443  p50=507  p90=1931 p99=2914 max=3412   min=468  p50=1762 p90=2049 p99=2244 max=2245
```

(`decode_us`/`map_us` n=115/117: `map_dmabuf` runs once per *yielded* frame,
and one steady-state access unit occasionally yields more than one frame
through B-frame reordering, so it has slightly more samples than `decode_us`
over the same 115 steady iterations.)

**`map_us` — the actual hardware-decode wait — is the largest of the three
numbers, not `decode_us`.** In the unpaced run its median (758 us) is 6x
`decode_us`'s median (122 us), and its p99 (3.85 ms) is 20x `decode_us`'s
p99 (190 us). This is the number the first draft of this section never
measured.

**Pacing changes `map_us`'s shape substantially, matching what the review
that caught this predicted from its own reproduction.** Paced to the real
16.667 ms (60 Hz) arrival cadence, `map_us`'s median drops to 9 us — the
hardware has usually already finished decoding by the time `map_dmabuf` asks
— but the tail does not go away: 29 of 117 steady samples (24.8%) still
exceed 1 ms, topping out at a 4.07 ms max. Unpaced, that fraction is 42 of
117 (35.9%). Read together: under real arrival timing, roughly one frame in
four still pays a multi-millisecond wait; the other three pay next to
nothing.

`decode_us` itself is essentially unaffected by pacing (steady p50 122-156
us, p99 190-306 us in both runs) — it is CPU-side packet handling, not the
part waiting on the GPU.

**`poll_us` is higher here than in either M2's steady-state number or the
first draft of this section, and for two compounding reasons, not one.**
First (unchanged from the first draft): `blit_h264_frame` calls
`ring.mark_dirty_all()` on every H.264 frame, so every `publish` here does a
full-frame blit, not M2's partial 64x64-region blit — the fair comparison is
M2's own three full-surface buffer-warmup samples (`[1163, 417, 1934] us`),
not its 97/624 us steady-state pair. Second (new in this round, per review
MODERATE 4): the first draft's harness passed `host_visible: true` to
`Renderer::new`, pinning export buffers to CPU-mappable memory; production
derives that flag from `Config::debug_map_frames`, which defaults `false` —
device-local memory. This section's numbers now use `host_visible: false`,
so `poll_us` here is finally measuring the same memory type `fb.blit_rects`
targets in production, closing that gap rather than just noting it.

**Per-iteration worst case, summing `decode_us` + that iteration's
`map_us` + `poll_us`:**

```
                              UNPACED                    PACED 60Hz
sum, steady (n=115)     p50=1794  p99=4608  max=5273   p50=2142  p99=5854  max=5856  (us)
% of 16.67 ms budget      11%       28%       32%         13%       35%       35%
```

This is meaningfully worse than the first draft's "~3.7 ms, 22% of budget"
claim, which omitted `map_us` entirely. It is also **not** the review's own
reproduction number (8-11.7 ms, 45-70%): that run's `map_us` distribution
(p50 1986 us, p99 7943 us, max 8046 us) ran roughly 2-3x higher than either
run captured here. Both reproductions agree on the *shape* — `map_us`
dominates, pacing helps the median but not the tail — but not on the exact
magnitude, on the same class of hardware, within the same investigation.
That gap is itself a finding: **this number has real run-to-run variance on
one machine**, wide enough that neither single run should be read as *the*
number. Nothing in either run exceeded the 16.67 ms budget on its own worst
sample, but the margin that remains is thin enough, and variable enough,
that "comfortably under" is not the right phrase for it (see §10.6).

### 10.5 The per-call allocations

Still not measured in isolation, and that conclusion is **not** overturned
by the sync-timing fix: `decode_us` (which does contain
`avcodec_send_packet`'s deep copy, `drain()`'s `AVFrame` allocations, and
the per-yielding-call `Vec`) stayed small and pacing-insensitive throughout
this round (steady p99 190-306 us) — the cost this section found to be
large, `map_us`, is a hardware wait, not an allocation, and moving to
`hw_decode.c`'s scratch-`AVFrame`-plus-`av_frame_move_ref` shape would not
touch it. Attributing the *entire* steady-state `decode_us` p99 (306 us,
the higher of the two runs) to allocation still puts the ceiling at 54x
under the 16.67 ms budget. **Still not worth moving to the `hw_decode.c`
shape on this evidence** — the finding this round changed is about the sync
wait, not about `decode_us`'s composition. What would change this specific
answer is unchanged from the first draft: concurrent sessions sharing one
render thread, or a future finding that allocation specifically (not decode
as a whole) shows allocator-contention symptoms under load.

### 10.6 Verdict: is a decode thread justified?

**Thinner margin than first reported, genuinely so — but still "no" on this
evidence, held more cautiously than the first draft held it.** The
corrected worst-observed per-iteration cost is 28-35% of the 16.67 ms budget
at p99/max in steady state (§10.4), driven almost entirely by `map_us` (the
`vaSyncSurface` wait the first draft never timed), not by `decode_us`. That
is a real finding, not noise: it moves this section from "comfortably
under" to "using a third of the budget at the tail, on one frame in four
under realistic pacing." It did not, in either run captured here, exceed the
budget on a single iteration. The review that caught this gap reproduced
`map_us` at roughly 2-3x this round's magnitude on nominally the same
hardware, which is reason to treat 35% as a floor on the real tail cost
here, not a ceiling.

Two things follow from that, not one:

1. **The one-time first-frame stall is worse than first reported and is
   still the clearer, higher-value fix.** ~43.5-46 ms (§10.2) against a
   16.67 ms budget is unambiguous regardless of steady-state variance;
   warming the VA-API context before the first real access unit arrives
   remains the recommended fix, now with roughly double the previously
   credited payoff.
2. **The steady-state question is closer than "no" implies and should not
   be read as closed.** A decode thread is not justified by *this*
   evidence — no measured iteration exceeded budget, and the dominant cost
   (`map_us`) is a hardware wait a decode thread would move off the render
   thread but not shrink. But the margin measured here (65-72% headroom at
   worst observed) is not the 78%+ headroom the first draft reported, the
   two reproductions of the same measurement disagree by 2-3x on the tail,
   and nothing here tested the condition most likely to close that
   remaining margin: concurrent sessions, other render-thread work stacked
   on top, or a less capable GPU than this dedicated RX 480. **What would
   flip the verdict:** a repeat of this measurement under any of those
   three conditions showing the `map_us` tail pushing the per-iteration sum
   materially past what is measured here — at the review's own 45-70%
   figure, a decode thread would be the honest conclusion, not a deferred
   one.

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

## 12.1 An environment gap worth closing

**No Vulkan validation layers are installed on the development machine**
(`/usr/share/vulkan/explicit_layer.d` is empty; `vulkaninfo` reports two
instance layers, neither Khronos validation). This was found while
mutation-testing Task 6: deliberately halving the `allocationSize` passed to
`vkAllocateMemory` produced **no observable effect** — RADV's dma-buf import
uses the fd's real backing size regardless — and nothing flagged the
out-of-bounds bind that would be. The code carries a `debug_assert_eq!` as the
only thing that catches it.

That is a blind spot for a milestone that imports external memory, binds two
images into one allocation at manual offsets, and hands raw `VkImage` handles
to wgpu. `vulkan-validationlayers` would catch exactly this class: bad bind
offsets, undersized allocations, layout-transition mistakes, and
use-after-destroy — the last of which is the hazard §C3 of Task 6's review
identified in `ImportedNv12`'s per-frame `Drop`.

Installing it is a system change and is not done here. Recommended before
Task 9 wires the import into the per-frame render path, where a
use-after-destroy would otherwise present as an intermittent GPU hang rather
than a message naming the object.

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
