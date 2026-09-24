# Native client M3: H.264 decode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decode the H.264 access units the native client already receives, get them onto the GPU, and advertise the capability so the server sends them.

**Architecture:** A new crate `ghostframe-client-h264` owns the ffmpeg/VA-API FFI and produces a plain `DmabufPlanes` struct. `ghostframe-client-gpu` imports those planes as two single-plane textures and converts them in WGSL. Neither crate knows about the other's internals, which is what makes the two exact oracles possible.

**Tech Stack:** ffmpeg-next `=9.0.0` + `ffmpeg-sys-next` raw FFI, VA-API through `AV_HWDEVICE_TYPE_VAAPI`, ash/Vulkan external memory import, wgpu 30 + WGSL.

**Spec:** `docs/superpowers/specs/2026-09-23-native-client-m3-design.md`

---

## Read this first

**Colour is decided and must not be re-derived.** The server encodes **full-range
BT.601** (`ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp`, which says so in
its own comment). The M1 spec's "BT.709 limited-range" is wrong. Every constant in
this plan comes from inverting that shader's actual matrix. If a test fails, the
answer is never "widen the tolerance" or "try BT.709".

**Exactness is not decoration.** H.264's inverse transform is specified exactly by
the standard, so hardware and software decoders produce *identical* NV12 for the
same bitstream. Task 5 asserts that with no tolerance. If it fails, something is
genuinely wrong with how we drive the decoder.

**The capability flip changes first paint.** `io_bridge.rs:3435` sets
`frame_mode = FrameMode::H264` on session reset. The moment the client advertises
H.264, its *first* frames are H.264. Task 10 flips it; everything must work first.

**Verify before you believe a blocker.** If a task seems impossible because
"the hardware can't do X", run a known-good path on the same machine before
reporting it. M1 lost time to a false BLOCKED of exactly this shape.

---

## File structure

| File | Responsibility | Task |
|---|---|---|
| `ghostframe-client-h264/Cargo.toml` | New crate manifest | 2 |
| `ghostframe-client-h264/src/lib.rs` | Error type, re-exports | 2 |
| `ghostframe-client-h264/src/probe.rs` | "Can this machine decode H.264 via VA-API?" | 2, 3 |
| `ghostframe-client-h264/src/probe_clip.h264` | 655-byte keyframe the probe decodes to prove it | 3 |
| `ghostframe-client-h264/src/decoder.rs` | `H264Decoder`: codec ctx, hw device ctx, send/receive | 3 |
| `ghostframe-client-h264/src/descriptor.rs` | `AVDRMFrameDescriptor` → `DmabufPlanes` | 4 |
| `ghostframe-client-h264/src/testclip.rs` | Test-only: encode a known clip with libx264 | 3 |
| `ghostframe-client-gpu/src/import.rs` | `DmabufPlanes` → two `wgpu::Texture` | 6 |
| `shaders/client/h264_nv12_blit.wgsl` | Full-range BT.601 inverse | 7 |
| `ghostframe-client-gpu/src/nv12_reference.rs` | The CPU reference the shader is asserted against | 7 |
| `ghostframe-client-gpu/src/pipelines/h264_nv12.rs` | Pipeline, bind groups, draw | 8 |
| `ghostframe-client-gpu/src/renderer.rs` | Replace the `NeedsH264` no-op arm | 9 |
| `ghostframe-client-native/src/lib.rs` | Probe at `Client::new`, effective capability | 10 |
| `ghostframe-client-capi/src/lib.rs` | `gf_client_supports_h264` getter | 10 |
| `ghostframe-cli/src/commands.rs` | Stop hardcoding `supports_h264: false` | 10 |
| `ghostframe-e2e/tests/h264.rs` | Acceptance against a live server | 11 |

---

## Task 1: Spike — what does VA-API actually hand back?

Everything in Task 6 branches on one unknown: does radeonsi export decode
surfaces **linear** or **tiled**? This RX 480 lacks
`VK_EXT_image_drm_format_modifier` (confirmed against `vulkaninfo`), so a tiled
surface cannot be imported into Vulkan here at all. Find out before writing code
that assumes an answer.

This task is throwaway. It produces a number written into the plan, not shipped code.

**Files:**
- Create: `/tmp/claude-1000/-home-cedric-work-ghostframe/eebfbb06-980b-4e7b-be07-0cdbcc8f6008/scratchpad/m3-spike/` (a standalone cargo project, NOT a workspace member)

- [ ] **Step 1: Create the spike project**

```bash
SPIKE=/tmp/claude-1000/-home-cedric-work-ghostframe/eebfbb06-980b-4e7b-be07-0cdbcc8f6008/scratchpad/m3-spike
mkdir -p $SPIKE/src
cat > $SPIKE/Cargo.toml <<'EOF'
[package]
name = "m3-spike"
version = "0.1.0"
edition = "2021"

[dependencies]
ffmpeg-next = "=9.0.0"
ffmpeg-sys-next = "=9.0.0"
libc = "0.2"

[workspace]
EOF
```

- [ ] **Step 2: Write the spike**

It encodes 3 frames with libx264 (so the spike needs no test asset), decodes them
through VA-API, maps the result to DRM_PRIME, and prints the descriptor.

```rust
// $SPIKE/src/main.rs
use ffmpeg_next as ffmpeg;
use ffmpeg_sys_next as ffi;
use std::ptr;

const W: u32 = 640;
const H: u32 = 480;

/// Encode `n` frames of a gradient with libx264; return Annex-B access units.
fn make_clip(n: usize) -> Vec<Vec<u8>> {
    ffmpeg::init().expect("ffmpeg init");
    let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264).expect("libx264 missing");
    let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
    let mut enc = ctx.encoder().video().expect("video encoder");
    enc.set_width(W);
    enc.set_height(H);
    enc.set_format(ffmpeg::format::Pixel::YUV420P);
    enc.set_time_base(ffmpeg::Rational::new(1, 60));
    let mut enc = enc.open_as(codec).expect("open libx264");

    let mut out = Vec::new();
    for i in 0..n {
        let mut f = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, W, H);
        for y in 0..H as usize {
            let stride = f.stride(0);
            let row = &mut f.data_mut(0)[y * stride..y * stride + W as usize];
            for (x, px) in row.iter_mut().enumerate() {
                *px = ((x + y + i * 16) % 256) as u8;
            }
        }
        for plane in 1..3 {
            let stride = f.stride(plane);
            let rows = (H / 2) as usize;
            for y in 0..rows {
                let row = &mut f.data_mut(plane)[y * stride..y * stride + (W / 2) as usize];
                row.fill(128);
            }
        }
        f.set_pts(Some(i as i64));
        enc.send_frame(&f).expect("send_frame");
        let mut pkt = ffmpeg::Packet::empty();
        while enc.receive_packet(&mut pkt).is_ok() {
            out.push(pkt.data().expect("packet data").to_vec());
        }
    }
    enc.send_eof().expect("send_eof");
    let mut pkt = ffmpeg::Packet::empty();
    while enc.receive_packet(&mut pkt).is_ok() {
        out.push(pkt.data().expect("packet data").to_vec());
    }
    out
}

unsafe extern "C" fn get_vaapi_format(
    _ctx: *mut ffi::AVCodecContext,
    mut fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    while *fmts != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *fmts == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
            return ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        }
        fmts = fmts.add(1);
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

fn main() {
    let clip = make_clip(3);
    println!("[spike] encoded {} access units", clip.len());

    unsafe {
        let mut hw_dev: *mut ffi::AVBufferRef = ptr::null_mut();
        let path = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        let ret = ffi::av_hwdevice_ctx_create(
            &mut hw_dev,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            path.as_ptr(),
            ptr::null_mut(),
            0,
        );
        assert!(ret >= 0, "av_hwdevice_ctx_create failed: {ret}");

        let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264);
        assert!(!codec.is_null(), "no h264 decoder");
        let ctx = ffi::avcodec_alloc_context3(codec);
        (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_dev);
        (*ctx).get_format = Some(get_vaapi_format);
        let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
        assert!(ret >= 0, "avcodec_open2 failed: {ret}");

        let frame = ffi::av_frame_alloc();
        let pkt = ffi::av_packet_alloc();

        for au in &clip {
            (*pkt).data = au.as_ptr() as *mut u8;
            (*pkt).size = au.len() as i32;
            let ret = ffi::avcodec_send_packet(ctx, pkt);
            assert!(ret >= 0, "send_packet: {ret}");
            loop {
                let ret = ffi::avcodec_receive_frame(ctx, frame);
                if ret < 0 {
                    break;
                }
                println!(
                    "[spike] frame {}x{} format={:?}",
                    (*frame).width,
                    (*frame).height,
                    std::mem::transmute::<i32, ffi::AVPixelFormat>((*frame).format)
                );

                let drm = ffi::av_frame_alloc();
                (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
                let ret = ffi::av_hwframe_map(
                    drm,
                    frame,
                    ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32,
                );
                if ret < 0 {
                    println!("[spike] av_hwframe_map FAILED: {ret}");
                } else {
                    let d = (*drm).data[0] as *const ffi::AVDRMFrameDescriptor;
                    println!("[spike] nb_objects={} nb_layers={}", (*d).nb_objects, (*d).nb_layers);
                    for o in 0..(*d).nb_objects as usize {
                        println!(
                            "[spike]   object[{o}] fd={} size={} modifier=0x{:016x}",
                            (*d).objects[o].fd,
                            (*d).objects[o].size,
                            (*d).objects[o].format_modifier
                        );
                    }
                    for l in 0..(*d).nb_layers as usize {
                        let layer = &(*d).layers[l];
                        println!(
                            "[spike]   layer[{l}] format=0x{:08x} nb_planes={}",
                            layer.format, layer.nb_planes
                        );
                        for p in 0..layer.nb_planes as usize {
                            println!(
                                "[spike]     plane[{p}] object_index={} offset={} pitch={}",
                                layer.planes[p].object_index,
                                layer.planes[p].offset,
                                layer.planes[p].pitch
                            );
                        }
                    }
                }
                ffi::av_frame_free(&mut (drm as *mut _));
            }
        }
        println!("[spike] DRM_FORMAT_MOD_LINEAR is 0x0000000000000000");
    }
}
```

- [ ] **Step 3: Run it**

```bash
cd /tmp/claude-1000/-home-cedric-work-ghostframe/eebfbb06-980b-4e7b-be07-0cdbcc8f6008/scratchpad/m3-spike && cargo run --release 2>&1 | grep '\[spike\]'
```

Expected: several `[spike] frame 640x480 format=AV_PIX_FMT_VAAPI` lines, then
object/layer/plane lines. The number that matters is `modifier=0x...`.

- [ ] **Step 4: Record the answer in the spec**

Append a subsection to `docs/superpowers/specs/2026-09-23-native-client-m3-design.md`
under §7 stating: the modifier value, whether it is `DRM_FORMAT_MOD_LINEAR` (0), the
object/layer/plane shape (one object with a 2-plane layer, or two layers), and the
plane offsets and pitches for 640x480. Write what was observed, not what was hoped.

If the modifier is **not** 0, also state that Task 6's direct import cannot work on
this GPU and Task 6's CPU fallback is the live path here. That is a finding, not a
failure — the design anticipates it.

- [ ] **Step 5: Commit the finding**

```bash
cd /home/cedric/work/ghostframe
git add docs/superpowers/specs/2026-09-23-native-client-m3-design.md
git commit -m "spike(m3): record the DRM modifier VA-API exports for H.264 decode"
```

---

## Task 2: The `ghostframe-client-h264` crate and its VA-API probe

**Files:**
- Create: `ghostframe-client-h264/Cargo.toml`, `ghostframe-client-h264/src/lib.rs`, `ghostframe-client-h264/src/probe.rs`
- Modify: `Cargo.toml` (workspace members), `tests/containers/test-server/Dockerfile`

- [ ] **Step 1: Write the failing test**

```rust
// ghostframe-client-h264/src/probe.rs  (tests at the bottom of the file)
#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must answer without panicking on any machine, including one
    /// with no GPU at all. It is called on the connect path, where a panic
    /// would take down a session over a missing optional feature.
    #[test]
    fn probe_returns_a_verdict_without_panicking() {
        let _verdict: bool = vaapi_h264_decode_available();
    }

    /// On a machine whose driver reports an H.264 decode entrypoint, the probe
    /// must say so.
    ///
    /// The gate is `vainfo`, NOT the mere existence of a render node. A node
    /// exists on machines whose Mesa was built without video codecs, where the
    /// correct answer is `false` -- gating on the node would demand the wrong
    /// answer there and entrench the very defect the probe exists to avoid.
    #[test]
    fn probe_agrees_with_the_driver() {
        let vainfo = std::process::Command::new("vainfo").output();
        let Ok(out) = vainfo else {
            eprintln!("vainfo not installed; cannot establish ground truth, skipping");
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let driver_decodes_h264 = text
            .lines()
            .any(|l| l.contains("VAProfileH264") && l.contains("VAEntrypointVLD"));
        if !driver_decodes_h264 {
            eprintln!("driver reports no H.264 VLD entrypoint; probe should say false");
            assert!(
                !vaapi_h264_decode_available(),
                "the driver reports no H.264 decode entrypoint, but the probe said yes -- \
                 this is the false positive that produces a black window on first paint"
            );
            return;
        }
        assert!(
            vaapi_h264_decode_available(),
            "`vainfo` reports a VAProfileH264*/VAEntrypointVLD pair but the probe says no"
        );
    }
}
```

- [ ] **Step 2: Create the crate manifest**

```toml
# ghostframe-client-h264/Cargo.toml
[package]
name = "ghostframe-client-h264"
version = "0.1.0"
edition = "2021"

[dependencies]
ffmpeg-next = { workspace = true }
ffmpeg-sys-next = { workspace = true }
libc = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
```

Check `ffmpeg-sys-next` is in the workspace `[workspace.dependencies]`; if it is
not, add `ffmpeg-sys-next = { version = "=9.0.0" }` next to the existing
`ffmpeg-next = { version = "=9.0.0" }` at `Cargo.toml:88`.

Add `"ghostframe-client-h264"` to the workspace `members` list in the root
`Cargo.toml`.

- [ ] **Step 3: Write `lib.rs`**

```rust
// ghostframe-client-h264/src/lib.rs
//! H.264 decode for the native client: ffmpeg + VA-API in, a dmabuf
//! description out.
//!
//! This crate owns every unsafe ffmpeg call in the client and knows nothing
//! about wgpu. The boundary is [`DmabufPlanes`], a plain struct with no
//! ffmpeg types in it. That is deliberate: it lets `ghostframe-client-gpu`
//! be tested with synthetic planes and no decoder, and lets this crate be
//! tested with no GPU surface.

pub mod decoder;
pub mod descriptor;
pub mod probe;

pub use descriptor::{DmabufPlanes, PlaneDesc};
pub use probe::vaapi_h264_decode_available;

#[derive(Debug, thiserror::Error)]
pub enum H264Error {
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),

    /// VA-API could not be opened. Not fatal anywhere in this client: the
    /// capability bit stays clear and the session runs on the tile codecs.
    #[error("VA-API unavailable: {0}")]
    VaapiUnavailable(String),

    #[error("decoded frame is {got_w}x{got_h}, expected {want_w}x{want_h}")]
    SizeMismatch {
        got_w: u32,
        got_h: u32,
        want_w: u32,
        want_h: u32,
    },

    #[error("unexpected DRM descriptor: {0}")]
    Descriptor(String),
}
```

- [ ] **Step 4: Write the probe**

```rust
// ghostframe-client-h264/src/probe.rs
//! Can this machine decode H.264 through VA-API?
//!
//! Called on the connect path, before HELLO is built, because the capability
//! byte is sent once at session start and never revised. A `false` here is
//! not an error: the server then sends tile codecs, which is what every
//! session did before M3.

use ffmpeg_sys_next as ffi;
use std::ffi::CString;
use std::ptr;

/// Default VA-API render node. Matches
/// `ghostframe-lib/src/encoder/vaapi_device.rs`'s `VAAPI_DEVICE`.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";

/// True when this machine can plausibly decode H.264 through VA-API.
///
/// **This is a necessary condition, not a sufficient one, and Task 3 replaces
/// it with a functional check.** `avcodec_find_decoder(AV_CODEC_ID_H264)`
/// returns libavcodec's *software* decoder and is entirely independent of
/// VA-API -- there is no `h264_vaapi` decoder, only an `h264` decoder with a
/// VA-API hwaccel. So the strongest thing reachable without decoding a frame
/// is: a VA-API device opens, AND libavcodec was built with a VA-API hwaccel
/// for H.264. A driver whose H.264 profile is missing entirely (Mesa built
/// without `video-codecs`, which several distributions shipped for years)
/// still passes this.
///
/// That gap matters because sessions begin in H.264 mode, so a false positive
/// is a black window rather than a degraded one. It is tolerable only because
/// nothing advertises the capability until Task 10, by which point Task 3 has
/// made this check functional.
///
/// Deliberately does NOT test whether the decoded surface can be imported
/// into Vulkan -- that is a separate question (see the design doc §7), and
/// conflating them would make an import bug look like missing hardware.
pub fn vaapi_h264_decode_available() -> bool {
    // ffmpeg logs libva failures straight to stderr at its default level. On a
    // machine with no VA-API -- the outcome this whole design calls normal --
    // that puts an unstructured error line on the host application's stderr,
    // which a C host embedding this library cannot suppress. Quiet it for the
    // duration of the probe and restore afterwards.
    // SAFETY: `av_log_get_level`/`av_log_set_level` are plain global accessors.
    let prior_log_level = unsafe {
        let prior = ffi::av_log_get_level();
        ffi::av_log_set_level(ffi::AV_LOG_QUIET);
        prior
    };
    let verdict = probe_inner();
    // SAFETY: as above; restores exactly what was read.
    unsafe { ffi::av_log_set_level(prior_log_level) };
    match &verdict {
        Ok(()) => true,
        Err(e) => {
            tracing::info!(reason = %e, "H.264 will not be advertised");
            false
        }
    }
}

/// The probe proper, returning WHY it failed.
///
/// Separate from the `bool` wrapper so the reason survives: `H264Error` is how
/// `gf_client_supports_h264`'s caller could one day learn the difference
/// between "no GPU", "permission denied on the render node", and "this driver
/// has no H.264 decode profile" -- three very different things for a user to
/// act on, and a bare `false` flattens them.
fn probe_inner() -> Result<(), H264Error> {
    let path = CString::new(RENDER_NODE)
        .map_err(|_| H264Error::VaapiUnavailable(format!("bad device path {RENDER_NODE:?}")))?;

    // RAII: `BufRef` unrefs on drop, so every early return below is leak-free
    // without repeating the cleanup. Mirrors
    // `ghostframe-lib/src/encoder/vaapi_device.rs`'s wrapper of the same name.
    let mut raw: *mut ffi::AVBufferRef = ptr::null_mut();
    // SAFETY: `path` outlives the call; `raw` is a valid out-param that ffmpeg
    // leaves null on failure, which the check below respects before any deref.
    let ret = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut raw,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            path.as_ptr(),
            ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        // Render the errno: -13 (permission -- not in the `render` group) and
        // -2 (no such device) are the two common causes and they need
        // completely different fixes. A bare negative integer tells a user
        // nothing.
        return Err(H264Error::VaapiUnavailable(format!(
            "av_hwdevice_ctx_create({RENDER_NODE}): {}",
            ffmpeg_next::Error::from(ret)
        )));
    }
    let _device = BufRef(raw);

    // SAFETY: a plain registry lookup with no ownership transfer.
    let codec = unsafe { ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264) };
    if codec.is_null() {
        return Err(H264Error::Ffmpeg("no H.264 decoder in ffmpeg".into()));
    }

    // Does libavcodec actually carry a VA-API hwaccel for H.264? Without this
    // the check above is satisfied by the software decoder on every build.
    let mut i = 0;
    loop {
        // SAFETY: `codec` is a valid static codec descriptor; ffmpeg returns
        // null past the end of the config list, which terminates the loop.
        let cfg = unsafe { ffi::avcodec_get_hw_config(codec, i) };
        if cfg.is_null() {
            return Err(H264Error::VaapiUnavailable(
                "libavcodec has no VA-API hwaccel for H.264 (built without it)".into(),
            ));
        }
        // SAFETY: `cfg` is non-null and points at a live static config.
        let (methods, device_type) = unsafe { ((*cfg).methods, (*cfg).device_type) };
        let has_device_ctx = methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32 != 0;
        if has_device_ctx && device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI {
            return Ok(());
        }
        i += 1;
    }
}
```

Add the RAII wrapper to `ghostframe-client-h264/src/lib.rs`:

```rust
/// RAII wrapper around `*mut AVBufferRef` so no early return leaks a hardware
/// device context. Mirrors `ghostframe-lib/src/encoder/vaapi_device.rs`'s
/// `BufRef`; this crate has its own because it is declared the owner of every
/// unsafe ffmpeg call in the client and should not reach into the server crate.
pub(crate) struct BufRef(pub *mut ffmpeg_sys_next::AVBufferRef);

impl Drop for BufRef {
    fn drop(&mut self) {
        // SAFETY: `AVBufferRef` is refcounted; this struct owns exactly one
        // reference and releases it exactly once.
        unsafe { ffmpeg_sys_next::av_buffer_unref(&mut self.0) };
    }
}
```

- [ ] **Step 5: Register the crate with the e2e container image**

A new workspace member breaks the test-server image, which copies manifests
per crate — `docker buildx` then fails before any test runs. Add these two
lines to `tests/containers/test-server/Dockerfile`, immediately after the
`ghostframe-cli` pair at lines 60-61:

```dockerfile
COPY ghostframe-client-h264/Cargo.toml ghostframe-client-h264/Cargo.toml
COPY ghostframe-client-h264/src/ ghostframe-client-h264/src/
```

- [ ] **Step 6: Run the tests**

```bash
cargo test -p ghostframe-client-h264
```

Expected: 2 passed. On this dev box `probe_agrees_with_the_render_node` asserts
rather than skipping, because `/dev/dri/renderD128` exists.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-client-h264 Cargo.toml Cargo.lock tests/containers/test-server/Dockerfile
git commit -m "feat(h264): new client-h264 crate with a VA-API decode probe"
```

---

## Task 3: The decoder

**Files:**
- Create: `ghostframe-client-h264/src/decoder.rs`, `ghostframe-client-h264/src/testclip.rs`
- Modify: `ghostframe-client-h264/src/lib.rs`, `ghostframe-client-h264/Cargo.toml`

- [ ] **Step 1: Write the test-clip helper**

Generating the clip in-process beats checking in a binary asset: it keeps the
repo free of opaque test data, and the encoder is already a dependency.

```rust
// ghostframe-client-h264/src/testclip.rs
//! Test-only: synthesize a known H.264 clip with libx264.
//!
//! Not `#[cfg(test)]` because the oracle in `ghostframe-e2e` and the
//! `client-gpu` tests need it too. Costs nothing in a release build beyond
//! the code size of one function.

use ffmpeg_next as ffmpeg;

/// Encode `n` frames of a moving gradient at `w`x`h`; return Annex-B access
/// units, one per frame.
///
/// The gradient matters: a flat colour compresses to almost nothing and
/// would exercise none of the decoder's transform paths.
pub fn gradient_clip(w: u32, h: u32, n: usize) -> Vec<Vec<u8>> {
    ffmpeg::init().expect("ffmpeg init");
    // `find_by_name`, not `find(Id::H264)`: the latter returns whichever H.264
    // encoder registers first, which can be `h264_vaapi` or `h264_nvenc`. Those
    // then fail on a YUV420P software frame with no hardware frames context,
    // while the `.expect` below claims libx264 is missing. Same house rule as
    // `ghostframe-lib/src/encoder/h264_vaapi.rs:159`.
    let codec = ffmpeg::encoder::find_by_name("libx264").expect("libx264 not available");
    let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
    let mut enc = ctx.encoder().video().expect("video encoder");
    enc.set_width(w);
    enc.set_height(h);
    enc.set_format(ffmpeg::format::Pixel::YUV420P);
    enc.set_time_base(ffmpeg::Rational::new(1, 60));
    let mut enc = enc.open_as(codec).expect("open libx264");

    let mut out = Vec::new();
    let mut drain = |enc: &mut ffmpeg::encoder::video::Encoder, out: &mut Vec<Vec<u8>>| {
        let mut pkt = ffmpeg::Packet::empty();
        while enc.receive_packet(&mut pkt).is_ok() {
            out.push(pkt.data().expect("packet data").to_vec());
        }
    };

    for i in 0..n {
        let mut f = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, w, h);
        let y_stride = f.stride(0);
        for y in 0..h as usize {
            let row = &mut f.data_mut(0)[y * y_stride..y * y_stride + w as usize];
            for (x, px) in row.iter_mut().enumerate() {
                *px = ((x + y + i * 16) % 256) as u8;
            }
        }
        for plane in 1..3 {
            let stride = f.stride(plane);
            for y in 0..(h / 2) as usize {
                let row = &mut f.data_mut(plane)[y * stride..y * stride + (w / 2) as usize];
                for (x, px) in row.iter_mut().enumerate() {
                    *px = ((x * 2 + plane * 40 + i * 8) % 256) as u8;
                }
            }
        }
        f.set_pts(Some(i as i64));
        enc.send_frame(&f).expect("send_frame");
        drain(&mut enc, &mut out);
    }
    enc.send_eof().expect("send_eof");
    drain(&mut enc, &mut out);
    out
}
```

- [ ] **Step 2: Write the failing decoder test**

```rust
// ghostframe-client-h264/src/decoder.rs  (tests at the bottom)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testclip::gradient_clip;

    fn skip_without_vaapi() -> bool {
        if !crate::probe::vaapi_h264_decode_available() {
            eprintln!("no VA-API H.264 decode here; skipping");
            return true;
        }
        false
    }

    #[test]
    fn decodes_a_clip_into_vaapi_surfaces() {
        if skip_without_vaapi() {
            return;
        }
        let clip = gradient_clip(640, 480, 5);
        assert!(!clip.is_empty(), "the test clip encoder produced nothing");

        let mut dec = H264Decoder::new().expect("open decoder");
        let mut frames = 0;
        for au in &clip {
            for frame in dec.decode(au).expect("decode") {
                assert_eq!(frame.width(), 640);
                assert_eq!(frame.height(), 480);
                frames += 1;
            }
        }
        for frame in dec.finish().expect("finish") {
            assert_eq!(frame.width(), 640);
            frames += 1;
        }
        assert_eq!(frames, 5, "expected one decoded frame per encoded frame");
    }

    /// A decoder fed garbage must report, not panic and not wedge. The
    /// transport can deliver a corrupt access unit whenever FEC fails to
    /// recover one.
    #[test]
    fn garbage_input_does_not_panic() {
        if skip_without_vaapi() {
            return;
        }
        let mut dec = H264Decoder::new().expect("open decoder");
        let _ = dec.decode(&[0x00, 0x00, 0x00, 0x01, 0xff, 0xff, 0xff]);
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-h264 decoder
```

Expected: FAIL to compile — `H264Decoder` does not exist.

- [ ] **Step 4: Implement the decoder**

```rust
// ghostframe-client-h264/src/decoder.rs
//! VA-API H.264 decode: access units in, hardware surfaces out.
//!
//! Mirrors ffmpeg's canonical `hw_decode.c`: set `hw_device_ctx`, override
//! `get_format` to pin AV_PIX_FMT_VAAPI, and let the decoder allocate its own
//! hardware frames context. The `get_format` override is what stops ffmpeg
//! silently falling back to software decode -- which would still *work*, and
//! would quietly cost a full-frame CPU download per frame.

use crate::H264Error;
use ffmpeg_sys_next as ffi;
use std::ffi::CString;
use std::ptr;

/// One decoded hardware frame. Owns its `AVFrame`.
pub struct HwFrame {
    pub(crate) frame: *mut ffi::AVFrame,
}

// SAFETY: `AVFrame` is refcounted and carries no thread affinity; this struct
// owns its pointer exclusively and frees it exactly once in `Drop`.
unsafe impl Send for HwFrame {}

impl HwFrame {
    pub fn width(&self) -> u32 {
        // SAFETY: `frame` is non-null and live for `self`'s lifetime.
        unsafe { (*self.frame).width as u32 }
    }

    pub fn height(&self) -> u32 {
        // SAFETY: as above.
        unsafe { (*self.frame).height as u32 }
    }
}

impl Drop for HwFrame {
    fn drop(&mut self) {
        // SAFETY: `frame` was allocated by `av_frame_alloc` and is dropped
        // exactly once here.
        unsafe { ffi::av_frame_free(&mut self.frame) };
    }
}

/// Pin AV_PIX_FMT_VAAPI out of the decoder's offered format list.
///
/// # Safety
/// Called by ffmpeg with a valid NUL-terminated (`AV_PIX_FMT_NONE`) format
/// array. Returning a format not in that list is undefined behaviour, so the
/// fallback returns `AV_PIX_FMT_NONE`, which ffmpeg treats as "cannot decode".
unsafe extern "C" fn get_vaapi_format(
    _ctx: *mut ffi::AVCodecContext,
    mut fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    while *fmts != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *fmts == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
            return ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        }
        fmts = fmts.add(1);
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

pub struct H264Decoder {
    ctx: *mut ffi::AVCodecContext,
    hw_device: *mut ffi::AVBufferRef,
    packet: *mut ffi::AVPacket,
}

// SAFETY: every pointer here is owned exclusively by this struct, freed once
// in `Drop`, and never shared. ffmpeg codec contexts are not thread-safe for
// concurrent use, which `&mut self` on every method already enforces.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new() -> Result<Self, H264Error> {
        Self::with_device(crate::probe::RENDER_NODE)
    }

    pub fn with_device(node: &str) -> Result<Self, H264Error> {
        let path = CString::new(node)
            .map_err(|_| H264Error::VaapiUnavailable(format!("bad device path {node:?}")))?;

        // SAFETY: all out-params are valid; every early return frees what it
        // has allocated so far, in reverse order of allocation.
        unsafe {
            let mut hw_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let ret = ffi::av_hwdevice_ctx_create(
                &mut hw_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                path.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if ret < 0 {
                return Err(H264Error::VaapiUnavailable(format!(
                    "av_hwdevice_ctx_create({node}) = {ret}"
                )));
            }

            let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg("no H.264 decoder in ffmpeg".into()));
            }

            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg("avcodec_alloc_context3 failed".into()));
            }
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device);
            (*ctx).get_format = Some(get_vaapi_format);

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                let mut c = ctx;
                ffi::avcodec_free_context(&mut c);
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg(format!("avcodec_open2 = {ret}")));
            }

            let packet = ffi::av_packet_alloc();
            if packet.is_null() {
                let mut c = ctx;
                ffi::avcodec_free_context(&mut c);
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg("av_packet_alloc failed".into()));
            }

            Ok(H264Decoder {
                ctx,
                hw_device,
                packet,
            })
        }
    }

    /// Feed one access unit; return every frame it completed.
    ///
    /// An empty result is normal, not an error: the decoder emits nothing
    /// until it has a keyframe, and B-frame reordering delays output.
    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<HwFrame>, H264Error> {
        // SAFETY: `au` outlives the `send_packet` call, which copies what it
        // needs; `self.packet` is a live allocation reset after every use.
        unsafe {
            (*self.packet).data = au.as_ptr() as *mut u8;
            (*self.packet).size = au.len() as i32;
            let ret = ffi::avcodec_send_packet(self.ctx, self.packet);
            (*self.packet).data = ptr::null_mut();
            (*self.packet).size = 0;
            if ret < 0 && ret != ffi::AVERROR(libc::EAGAIN) {
                return Err(H264Error::Ffmpeg(format!("send_packet = {ret}")));
            }
        }
        self.drain()
    }

    /// Signal end of stream and drain what the decoder still holds.
    ///
    /// **Terminal.** After this the decoder returns `AVERROR_EOF` for every
    /// subsequent packet; use [`H264Decoder::reset`] to make it usable again.
    /// Deliberately NOT called `flush`: ffmpeg's `avcodec_flush_buffers` means
    /// the opposite thing (discard state and continue), and `reset` below is
    /// the wrapper for that.
    pub fn finish(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        // SAFETY: a null packet is ffmpeg's documented end-of-stream signal.
        unsafe {
            let ret = ffi::avcodec_send_packet(self.ctx, ptr::null());
            if ret < 0 && ret != ffi::AVERROR_EOF {
                return Err(H264Error::Ffmpeg(format!("send_packet(NULL) = {ret}")));
            }
        }
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        let mut out = Vec::new();
        loop {
            // SAFETY: `av_frame_alloc` returns an owned frame or null; the
            // frame is either moved into `out` or freed before we return.
            let frame = unsafe { ffi::av_frame_alloc() };
            if frame.is_null() {
                return Err(H264Error::Ffmpeg("av_frame_alloc failed".into()));
            }
            // SAFETY: `self.ctx` is open; `frame` is a fresh allocation.
            let ret = unsafe { ffi::avcodec_receive_frame(self.ctx, frame) };
            if ret < 0 {
                // SAFETY: nothing took ownership of `frame`.
                unsafe { ffi::av_frame_free(&mut { frame }) };
                if ret == ffi::AVERROR(libc::EAGAIN) || ret == ffi::AVERROR_EOF {
                    return Ok(out);
                }
                return Err(H264Error::Ffmpeg(format!("receive_frame = {ret}")));
            }
            out.push(HwFrame { frame });
        }
    }
}

    /// Discard buffered state and continue decoding — ffmpeg's
    /// `avcodec_flush_buffers`.
    ///
    /// Task 9 needs this on stream discontinuity: unrecovered loss, a
    /// resolution change, or a session reset. Without it the decoder keeps
    /// trying to reference frames that will never arrive, and every output
    /// until the next keyframe is built on stale references.
    pub fn reset(&mut self) {
        // SAFETY: `self.ctx` is an open codec context owned solely by `self`.
        unsafe { ffi::avcodec_flush_buffers(self.ctx) };
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // SAFETY: freeing in reverse allocation order; each pointer is owned
        // solely by this struct and freed exactly once.
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.hw_device);
        }
    }
}
```

Add `pub mod testclip;` to `lib.rs`.

- [ ] **Step 5: Make the probe functional**

Task 2's probe establishes only a necessary condition: a VA-API device opens
and libavcodec carries a VA-API hwaccel for H.264. It cannot see whether the
*driver* has an H.264 decode profile — Mesa built without `video-codecs` passes
it — and a false positive there is a black window on first paint, because
sessions begin in H.264 mode. Now that `H264Decoder` exists, the probe can
answer the question by doing the thing.

Generate a tiny clip to decode. 64x64 is 655 bytes and decodes fine on VA-API
here, and embedding it beats generating one at runtime: no dependency on
libx264 being in the host's ffmpeg build, and no encode cost on the connect
path.

```bash
cd /home/cedric/work/ghostframe
ffmpeg -hide_banner -loglevel error -f lavfi -i color=c=gray:s=64x64:d=1 \
  -frames:v 1 -c:v libx264 -preset veryfast -profile:v high -f h264 \
  -y ghostframe-client-h264/src/probe_clip.h264
ls -l ghostframe-client-h264/src/probe_clip.h264   # expect ~758 bytes
ffprobe -loglevel error -show_entries stream=profile -of csv=p=0 \
  ghostframe-client-h264/src/probe_clip.h264        # MUST print: High
```

Replace `probe_inner`'s final `Ok(())` — the one returned when the hwaccel
config matches — with an actual decode. Append to `probe.rs`:

```rust
/// A 64x64 gray H.264 keyframe, Annex-B, ~655 bytes. Regenerate with:
///
/// ```text
/// ffmpeg -f lavfi -i color=c=gray:s=64x64:d=1 -frames:v 1 \
///   -c:v libx264 -preset ultrafast -f h264 -y src/probe_clip.h264
/// ```
///
/// Embedded rather than encoded at runtime so the probe does not depend on
/// libx264 being present in the host's ffmpeg build, and costs no encode on
/// the connect path.
///
/// **It must be High profile, because that is what the server sends.**
/// `h264_vaapi.rs` sets no profile, so ffmpeg's `h264_vaapi` encoder defaults
/// to High, and VA-API advertises ConstrainedBaseline / Main / High as
/// separate decode profiles. Probing with a Constrained Baseline clip would
/// reject a High-only driver that can decode the real stream perfectly well.
/// Note `-preset ultrafast` cannot produce High — it disables CABAC and 8x8
/// DCT, which are the features that make it High — so the preset above is
/// `veryfast`, and the `ffprobe` check is there because the profile is not
/// what the `-profile:v` flag alone determines.
const PROBE_CLIP: &[u8] = include_bytes!("probe_clip.h264");

/// Decode one frame through VA-API. The only check that actually proves the
/// driver can do it.
fn probe_decodes_a_frame() -> Result<(), H264Error> {
    let mut decoder = crate::decoder::H264Decoder::new()?;
    let mut frames = decoder.decode(PROBE_CLIP)?;
    frames.extend(decoder.finish()?);
    let frame = frames.first().ok_or_else(|| {
        H264Error::VaapiUnavailable(
            "VA-API accepted the stream but produced no frame (driver likely has no \
             H.264 decode profile)"
                .into(),
        )
    })?;
    if frame.width() == 0 || frame.height() == 0 {
        return Err(H264Error::VaapiUnavailable(
            "decoded probe frame has zero extent".into(),
        ));
    }
    Ok(())
}
```

and change the hwaccel-match arm in `probe_inner` from `return Ok(());` to
`return probe_decodes_a_frame();`.

Update `vaapi_h264_decode_available`'s doc comment: delete the paragraph
calling it "a necessary condition, not a sufficient one" and the paragraph
about the Task 3 replacement, and say instead that it decodes a 64x64 keyframe
through VA-API and returns whether a hardware frame came out.

- [ ] **Step 6: Confirm the probe still agrees with the driver**

```bash
cargo test -p ghostframe-client-h264 probe -- --nocapture
```

Expected: both probe tests pass. `probe_agrees_with_the_driver` gates on
`vainfo`'s entrypoint list, so it now checks a functional probe against the
driver's own claim rather than against the existence of a device node.

- [ ] **Step 7: Prove the probe is load-bearing**

Temporarily point `RENDER_NODE` at a path that is not a VA-API device (e.g.
`/dev/null`), run the probe tests, and confirm `vaapi_h264_decode_available`
returns false rather than panicking or hanging. Revert. This is the path a
machine without VA-API takes, and it must be boring.

- [ ] **Step 8: Run the tests**

```bash
cargo test -p ghostframe-client-h264
```

Expected: 4 passed (2 probe + 2 decoder).

- [ ] **Step 9: Commit**

```bash
git add ghostframe-client-h264
git commit -m "feat(h264): VA-API decoder, and a probe that decodes to prove it"
```

---

## Task 4: The DRM descriptor

**Files:**
- Create: `ghostframe-client-h264/src/descriptor.rs`
- Modify: `ghostframe-client-h264/src/decoder.rs` (add `HwFrame::map_dmabuf`)

- [ ] **Step 1: Write the failing test**

```rust
// ghostframe-client-h264/src/descriptor.rs  (tests at the bottom)
#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg_sys_next as ffi;

    /// Build a descriptor by hand in the shape VA-API uses for NV12: one
    /// object, one layer, two planes.
    fn one_object_two_planes() -> ffi::AVDRMFrameDescriptor {
        let mut d: ffi::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
        d.nb_objects = 1;
        d.objects[0].fd = 7;
        d.objects[0].size = 1024 * 768 * 3 / 2;
        d.objects[0].format_modifier = 0;
        d.nb_layers = 1;
        d.layers[0].nb_planes = 2;
        d.layers[0].planes[0].object_index = 0;
        d.layers[0].planes[0].offset = 0;
        d.layers[0].planes[0].pitch = 1024;
        d.layers[0].planes[1].object_index = 0;
        d.layers[0].planes[1].offset = 1024 * 768;
        d.layers[0].planes[1].pitch = 1024;
        d
    }

    /// The other shape drivers use: two layers of one plane each.
    fn two_layers_one_plane_each() -> ffi::AVDRMFrameDescriptor {
        let mut d: ffi::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
        d.nb_objects = 1;
        d.objects[0].fd = 9;
        d.objects[0].size = 1024 * 768 * 3 / 2;
        d.objects[0].format_modifier = 0;
        d.nb_layers = 2;
        d.layers[0].nb_planes = 1;
        d.layers[0].planes[0].object_index = 0;
        d.layers[0].planes[0].offset = 0;
        d.layers[0].planes[0].pitch = 1024;
        d.layers[1].nb_planes = 1;
        d.layers[1].planes[0].object_index = 0;
        d.layers[1].planes[0].offset = 1024 * 768;
        d.layers[1].planes[0].pitch = 1024;
        d
    }

    #[test]
    fn reads_the_single_layer_shape() {
        let d = one_object_two_planes();
        // SAFETY: `d` is a fully initialized descriptor living on this stack
        // frame for the duration of the call.
        let planes = unsafe { DmabufPlanes::from_descriptor(&d, 1024, 768) }.expect("parse");
        assert_eq!(planes.fd, 7);
        assert_eq!(planes.modifier, 0);
        assert_eq!(planes.luma.offset, 0);
        assert_eq!(planes.luma.pitch, 1024);
        assert_eq!(planes.chroma.offset, 1024 * 768);
        assert_eq!(planes.chroma.pitch, 1024);
        assert_eq!(planes.width, 1024);
        assert_eq!(planes.height, 768);
    }

    #[test]
    fn reads_the_two_layer_shape_identically() {
        let a = one_object_two_planes();
        let b = two_layers_one_plane_each();
        // SAFETY: both descriptors are fully initialized and live here.
        let pa = unsafe { DmabufPlanes::from_descriptor(&a, 1024, 768) }.expect("parse a");
        let pb = unsafe { DmabufPlanes::from_descriptor(&b, 1024, 768) }.expect("parse b");
        assert_eq!(pa.luma, pb.luma);
        assert_eq!(pa.chroma, pb.chroma);
    }

    /// Two objects means the planes live in separate dmabufs. The import
    /// path assumes one fd, so this must be an error rather than silently
    /// reading plane 1 from the wrong buffer.
    #[test]
    fn rejects_a_multi_object_descriptor() {
        let mut d = one_object_two_planes();
        d.nb_objects = 2;
        d.objects[1].fd = 8;
        // SAFETY: `d` is fully initialized.
        let err = unsafe { DmabufPlanes::from_descriptor(&d, 1024, 768) };
        assert!(err.is_err(), "a 2-object descriptor must not parse as one fd");
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-h264 descriptor
```

Expected: FAIL to compile — `DmabufPlanes` does not exist.

- [ ] **Step 3: Implement the descriptor**

```rust
// ghostframe-client-h264/src/descriptor.rs
//! `AVDRMFrameDescriptor` → a plain struct with no ffmpeg types in it.
//!
//! This is the crate boundary: `ghostframe-client-gpu` consumes
//! [`DmabufPlanes`] and never links ffmpeg.
//!
//! Drivers describe an NV12 dmabuf in two equivalent shapes -- one layer
//! with two planes, or two layers with one plane each -- and which one you
//! get is not something to depend on. Both are normalised here.

use crate::H264Error;
use ffmpeg_sys_next as ffi;

/// One plane's position inside the dmabuf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneDesc {
    pub offset: u64,
    pub pitch: u64,
}

/// An NV12 dmabuf: one fd, luma plane, chroma plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufPlanes {
    /// Borrowed from the mapped `AVFrame`, which owns it, and valid only while
    /// that `MappedFrame` is alive.
    ///
    /// **An importer must `dup()` this before any call that takes ownership.**
    /// `vkImportMemoryFdKHR` takes ownership: the fd is closed by
    /// `vkFreeMemory`, so handing this one over directly double-closes it
    /// against the `AVFrame`'s own unref. The design imports the same dmabuf
    /// twice (luma and chroma planes), which would be two closes of one fd.
    /// `import.rs` duplicates for exactly this reason.
    pub fd: i32,
    pub modifier: u64,
    /// DISPLAY dimensions (`AVFrame::width`/`height`), not the coded size.
    /// The alignment padding lives in `PlaneDesc::pitch`, which is why both
    /// are carried separately: a 640-wide frame here has pitch 768.
    pub width: u32,
    pub height: u32,
    pub luma: PlaneDesc,
    /// `width.div_ceil(2)` x `height.div_ceil(2)` samples, two bytes each (U
    /// and V interleaved). `div_ceil`, not `/ 2`: this client is deliberately
    /// tested at non-16-aligned resolutions, where truncation loses the last
    /// chroma column.
    pub chroma: PlaneDesc,
}

impl DmabufPlanes {
    /// # Safety
    /// `d` must be a fully initialized descriptor that outlives the call.
    pub unsafe fn from_descriptor(
        d: &ffi::AVDRMFrameDescriptor,
        width: u32,
        height: u32,
    ) -> Result<Self, H264Error> {
        if d.nb_objects != 1 {
            return Err(H264Error::Descriptor(format!(
                "expected 1 dmabuf object, got {} -- the import path binds one fd",
                d.nb_objects
            )));
        }

        // Flatten however the driver split the planes across layers.
        let mut flat: Vec<PlaneDesc> = Vec::new();
        for l in 0..d.nb_layers as usize {
            let layer = &d.layers[l];
            for p in 0..layer.nb_planes as usize {
                if layer.planes[p].object_index != 0 {
                    return Err(H264Error::Descriptor(format!(
                        "plane references object {}, but only object 0 was imported",
                        layer.planes[p].object_index
                    )));
                }
                flat.push(PlaneDesc {
                    offset: layer.planes[p].offset as u64,
                    pitch: layer.planes[p].pitch as u64,
                });
            }
        }

        if flat.len() != 2 {
            return Err(H264Error::Descriptor(format!(
                "expected 2 planes for NV12, got {}",
                flat.len()
            )));
        }

        Ok(DmabufPlanes {
            fd: d.objects[0].fd,
            modifier: d.objects[0].format_modifier,
            width,
            height,
            luma: flat[0],
            chroma: flat[1],
        })
    }
}
```

- [ ] **Step 4: Add the mapping method to `HwFrame`**

Append to `ghostframe-client-h264/src/decoder.rs`:

```rust
/// A hardware frame mapped to DRM_PRIME. Holds the mapped `AVFrame` alive,
/// because the fd inside [`DmabufPlanes`] is only valid while it lives.
pub struct MappedFrame {
    drm: *mut ffi::AVFrame,
    pub planes: crate::DmabufPlanes,
}

// SAFETY: exclusive ownership of `drm`, freed exactly once in `Drop`.
unsafe impl Send for MappedFrame {}

impl Drop for MappedFrame {
    fn drop(&mut self) {
        // SAFETY: allocated by `av_frame_alloc` in `map_dmabuf`.
        unsafe { ffi::av_frame_free(&mut self.drm) };
    }
}

impl HwFrame {
    /// Map this hardware surface to a DRM_PRIME dmabuf description.
    ///
    /// The returned [`MappedFrame`] owns the mapping; `planes.fd` is valid
    /// exactly as long as it lives, and must not be closed by the caller.
    pub fn map_dmabuf(&self) -> Result<MappedFrame, H264Error> {
        // SAFETY: `self.frame` is a live VAAPI frame. `drm` is freed on every
        // error path before returning; on success `MappedFrame` owns it.
        unsafe {
            let drm = ffi::av_frame_alloc();
            if drm.is_null() {
                return Err(H264Error::Ffmpeg("av_frame_alloc failed".into()));
            }
            (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let ret = ffi::av_hwframe_map(
                drm,
                self.frame,
                ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32,
            );
            if ret < 0 {
                ffi::av_frame_free(&mut { drm });
                return Err(H264Error::Ffmpeg(format!("av_hwframe_map = {ret}")));
            }
            let desc = (*drm).data[0] as *const ffi::AVDRMFrameDescriptor;
            if desc.is_null() {
                ffi::av_frame_free(&mut { drm });
                return Err(H264Error::Descriptor("mapped frame has no descriptor".into()));
            }
            match crate::DmabufPlanes::from_descriptor(&*desc, self.width(), self.height()) {
                Ok(planes) => Ok(MappedFrame { drm, planes }),
                Err(e) => {
                    ffi::av_frame_free(&mut { drm });
                    Err(e)
                }
            }
        }
    }
}
```

- [ ] **Step 5: Add a live mapping test**

Append to `decoder.rs`'s test module:

```rust
    /// The real descriptor from real hardware. Records the modifier in the
    /// failure message so a tiled surface names itself rather than showing
    /// up later as corrupted pixels.
    #[test]
    fn maps_a_decoded_frame_to_a_dmabuf() {
        if skip_without_vaapi() {
            return;
        }
        let clip = gradient_clip(640, 480, 3);
        let mut dec = H264Decoder::new().expect("open decoder");
        let mut mapped = 0;
        for au in &clip {
            for frame in dec.decode(au).expect("decode") {
                let m = frame.map_dmabuf().expect("map to dmabuf");
                assert!(m.planes.fd >= 0, "dmabuf fd must be valid");
                assert_eq!(m.planes.width, 640);
                assert_eq!(m.planes.height, 480);
                assert!(
                    m.planes.luma.pitch >= 640,
                    "luma pitch {} is narrower than the frame",
                    m.planes.luma.pitch
                );
                eprintln!(
                    "[m3] modifier=0x{:016x} luma(off={},pitch={}) chroma(off={},pitch={})",
                    m.planes.modifier,
                    m.planes.luma.offset,
                    m.planes.luma.pitch,
                    m.planes.chroma.offset,
                    m.planes.chroma.pitch
                );
                mapped += 1;
            }
        }
        assert!(mapped > 0, "no frame was mapped");
    }
```

- [ ] **Step 6: Run the tests**

```bash
cargo test -p ghostframe-client-h264 -- --nocapture 2>&1 | grep -E '\[m3\]|test result'
```

Expected: all pass, and the `[m3]` line prints the same modifier Task 1 recorded.
**If they disagree, stop and find out why** — one of the two is measuring
something other than what it claims.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-client-h264
git commit -m "feat(h264): normalise the DRM descriptor into DmabufPlanes"
```

---

## Task 5: Oracle 1 — hardware decode equals software decode, exactly

**Files:**
- Create: `ghostframe-client-h264/tests/oracle_decode.rs`

H.264's inverse transform is normative, so a conforming hardware decoder and
libavcodec's software decoder produce **identical** NV12. No tolerance.

The one wrinkle: software decode yields YUV420P (three planes, U and V separate)
while VA-API yields NV12 (two planes, U and V interleaved). The comparison must
interleave the software output first. That is an exact, lossless rearrangement —
**not** a conversion, and not a place for a tolerance.

- [ ] **Step 1: Write the failing test**

```rust
// ghostframe-client-h264/tests/oracle_decode.rs
//! Oracle: VA-API decode == software decode, byte for byte.
//!
//! Requires VA-API hardware. Deliberately NOT named in any CI workflow --
//! runners have no GPU. An #[ignore] would hide it on developer machines
//! too, which is where it has to run.

use ffmpeg_next as ffmpeg;
use ffmpeg_sys_next as ffi;
use ghostframe_client_h264::decoder::H264Decoder;
use ghostframe_client_h264::testclip::gradient_clip;

const W: u32 = 640;
const H: u32 = 480;

/// Decode with libavcodec's software H.264 decoder; return NV12 planes per
/// frame as (luma, chroma), tightly packed at W and W bytes per row.
fn software_decode_nv12(clip: &[Vec<u8>]) -> Vec<(Vec<u8>, Vec<u8>)> {
    ffmpeg::init().expect("ffmpeg init");
    let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264).expect("no h264 decoder");
    let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
    let mut dec = ctx.decoder().video().expect("video decoder");

    let mut out = Vec::new();
    let mut take = |dec: &mut ffmpeg::decoder::Video, out: &mut Vec<(Vec<u8>, Vec<u8>)>| {
        let mut frame = ffmpeg::frame::Video::empty();
        while dec.receive_frame(&mut frame).is_ok() {
            let y_stride = frame.stride(0);
            let mut luma = Vec::with_capacity((W * H) as usize);
            for row in 0..H as usize {
                luma.extend_from_slice(&frame.data(0)[row * y_stride..row * y_stride + W as usize]);
            }
            // YUV420P -> NV12: interleave U and V. Exact, not a conversion.
            let u_stride = frame.stride(1);
            let v_stride = frame.stride(2);
            let mut chroma = Vec::with_capacity((W * H / 2) as usize);
            for row in 0..(H / 2) as usize {
                let u = &frame.data(1)[row * u_stride..row * u_stride + (W / 2) as usize];
                let v = &frame.data(2)[row * v_stride..row * v_stride + (W / 2) as usize];
                for i in 0..(W / 2) as usize {
                    chroma.push(u[i]);
                    chroma.push(v[i]);
                }
            }
            out.push((luma, chroma));
        }
    };

    for au in clip {
        let pkt = ffmpeg::Packet::copy(au);
        dec.send_packet(&pkt).expect("send_packet");
        take(&mut dec, &mut out);
    }
    dec.send_eof().expect("send_eof");
    take(&mut dec, &mut out);
    out
}

/// Download a VA-API surface to system memory as NV12, tightly packed.
fn hw_frame_to_nv12(frame: &ghostframe_client_h264::decoder::HwFrame) -> (Vec<u8>, Vec<u8>) {
    // SAFETY: `frame` holds a live VAAPI AVFrame; `sw` is freed before return.
    unsafe {
        let sw = ffi::av_frame_alloc();
        assert!(!sw.is_null(), "av_frame_alloc");
        (*sw).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        let ret = ffi::av_hwframe_transfer_data(sw, frame.as_ptr(), 0);
        assert!(ret >= 0, "av_hwframe_transfer_data = {ret}");

        let y_stride = (*sw).linesize[0] as usize;
        let uv_stride = (*sw).linesize[1] as usize;
        let mut luma = Vec::with_capacity((W * H) as usize);
        for row in 0..H as usize {
            let p = (*sw).data[0].add(row * y_stride);
            luma.extend_from_slice(std::slice::from_raw_parts(p, W as usize));
        }
        let mut chroma = Vec::with_capacity((W * H / 2) as usize);
        for row in 0..(H / 2) as usize {
            let p = (*sw).data[1].add(row * uv_stride);
            chroma.extend_from_slice(std::slice::from_raw_parts(p, W as usize));
        }
        ffi::av_frame_free(&mut { sw });
        (luma, chroma)
    }
}

#[test]
fn hardware_decode_matches_software_decode_exactly() {
    if !ghostframe_client_h264::vaapi_h264_decode_available() {
        eprintln!("no VA-API H.264 decode here; skipping");
        return;
    }

    let clip = gradient_clip(W, H, 8);
    let sw = software_decode_nv12(&clip);

    let mut dec = H264Decoder::new().expect("open hw decoder");
    let mut hw = Vec::new();
    for au in &clip {
        for frame in dec.decode(au).expect("decode") {
            hw.push(hw_frame_to_nv12(&frame));
        }
    }
    for frame in dec.finish().expect("finish") {
        hw.push(hw_frame_to_nv12(&frame));
    }

    assert_eq!(hw.len(), sw.len(), "frame counts differ");
    assert!(!hw.is_empty(), "nothing decoded");

    for (i, ((hw_y, hw_uv), (sw_y, sw_uv))) in hw.iter().zip(sw.iter()).enumerate() {
        let y_diff = hw_y.iter().zip(sw_y).filter(|(a, b)| a != b).count();
        let uv_diff = hw_uv.iter().zip(sw_uv).filter(|(a, b)| a != b).count();
        assert_eq!(
            (y_diff, uv_diff),
            (0, 0),
            "frame {i}: hardware and software decode disagree on {y_diff} luma and \
             {uv_diff} chroma bytes. H.264's inverse transform is specified exactly, \
             so conforming decoders cannot differ -- this is a real bug in how the \
             hardware decoder is driven, not codec noise. Do NOT add a tolerance."
        );
    }
}
```

- [ ] **Step 2: Add the accessor the test needs**

In `ghostframe-client-h264/src/decoder.rs`, add to `impl HwFrame`:

```rust
    /// Raw pointer to the underlying frame, for callers that need ffmpeg APIs
    /// this crate does not wrap (the decode oracle downloads through
    /// `av_hwframe_transfer_data`). The frame stays owned by `self`.
    pub fn as_ptr(&self) -> *const ffi::AVFrame {
        self.frame
    }
```

- [ ] **Step 3: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-h264 --test oracle_decode
```

Expected: FAIL to compile — `as_ptr` does not exist until Step 2 is applied;
after Step 2, it must PASS.

- [ ] **Step 4: Run it and confirm it passes**

```bash
cargo test -p ghostframe-client-h264 --test oracle_decode -- --nocapture
```

Expected: `test result: ok. 1 passed`.

- [ ] **Step 5: Prove the oracle is load-bearing**

A test that passes for the wrong reason is worse than no test. Temporarily
corrupt one byte of the hardware output inside `hw_frame_to_nv12` — e.g. add
`luma[0] = luma[0].wrapping_add(1);` before returning — re-run, and confirm it
FAILS naming frame 0. Then revert the corruption.

- [ ] **Step 6: Settle whether the dmabuf is actually linear**

Spec §7.1: on this GPU (GFX8) the DRM modifier is structurally always
`INVALID`, so it says nothing about tiling, and the plane arithmetic cannot
distinguish linear from 1D/micro-tiled. This decides it, and it needs no GPU
API at all — just a memory map and the decoder's own authoritative download.

Add to `ghostframe-client-h264/tests/oracle_decode.rs`:

```rust
/// Is the exported dmabuf laid out exactly as its descriptor claims?
///
/// The modifier field cannot answer this on GFX8 (spec §7.1), so compare the
/// bytes directly: `av_hwframe_transfer_data` is authoritative, and an mmap of
/// the dmabuf at the descriptor's offsets and pitches must match it if — and
/// only if — the surface is linear at that layout.
///
/// A match means `import_nv12`'s LINEAR path is sound here despite the missing
/// metadata. A mismatch means tiled, and the CPU copy is confirmed on evidence
/// rather than on an absent field.
#[test]
fn the_exported_dmabuf_is_linear_at_the_descriptors_layout() {
    if !ghostframe_client_h264::vaapi_h264_decode_available() {
        eprintln!("no VA-API H.264 decode here; skipping");
        return;
    }

    let clip = gradient_clip(W, H, 3);
    let mut dec = H264Decoder::new().expect("open hw decoder");
    let mut frames = Vec::new();
    for au in &clip {
        frames.extend(dec.decode(au).expect("decode"));
    }
    frames.extend(dec.finish().expect("finish"));
    let frame = frames.first().expect("no frame decoded");

    // Authoritative pixels.
    let (want_luma, want_chroma) = hw_frame_to_nv12(frame);

    // The same surface, seen as raw memory.
    let mapped = frame.map_dmabuf().expect("map to dmabuf");
    let p = mapped.planes();
    let len = (p.chroma.offset + p.chroma.pitch * (H as u64 / 2)) as usize;

    // SAFETY: `p.fd` is a live dmabuf owned by `mapped`, and `len` is within
    // the object size the descriptor reports. Read-only, shared.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            p.fd,
            0,
        )
    };
    assert!(
        ptr != libc::MAP_FAILED,
        "mmap of the decoder's dmabuf failed: {}. Without a CPU mapping this \
         question cannot be settled from this test.",
        std::io::Error::last_os_error()
    );
    // SAFETY: `ptr` is a valid mapping of `len` bytes, live until munmap below.
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };

    let mut luma_diff = 0usize;
    for y in 0..H as usize {
        let row = &bytes[y * p.luma.pitch as usize..y * p.luma.pitch as usize + W as usize];
        luma_diff += row
            .iter()
            .zip(&want_luma[y * W as usize..(y + 1) * W as usize])
            .filter(|(a, b)| a != b)
            .count();
    }

    let mut chroma_diff = 0usize;
    for y in 0..(H / 2) as usize {
        let off = p.chroma.offset as usize + y * p.chroma.pitch as usize;
        let row = &bytes[off..off + W as usize];
        chroma_diff += row
            .iter()
            .zip(&want_chroma[y * W as usize..(y + 1) * W as usize])
            .filter(|(a, b)| a != b)
            .count();
    }

    // SAFETY: `ptr`/`len` are exactly what mmap returned and nothing else
    // holds the mapping.
    unsafe { libc::munmap(ptr, len) };

    let total = (W * H) as usize + (W * H / 2) as usize;
    eprintln!(
        "[m3] dmabuf-vs-download: {} of {} bytes differ (luma {}, chroma {})",
        luma_diff + chroma_diff,
        total,
        luma_diff,
        chroma_diff
    );

    // Deliberately NOT an assertion of linearity: both outcomes are valid
    // findings, and which one holds decides whether `import_nv12` can take the
    // LINEAR path on this hardware. What IS asserted is that the comparison
    // actually ran over real data -- a silently empty comparison would report
    // "0 differ" and look like success.
    assert!(total > 0 && !want_luma.is_empty(), "nothing was compared");
}
```

Add `libc = { workspace = true }` to `ghostframe-client-h264`'s
`[dev-dependencies]` if it is not already a normal dependency.

Run it and **record the byte-difference count in spec §7.1**, replacing the
"unmeasured" language with the measurement:

```bash
cargo test -p ghostframe-client-h264 --test oracle_decode -- --nocapture 2>&1 | grep '\[m3\]'
```

- **0 differ** → the surface is linear at the descriptor's layout. Say so, and
  note that `import_nv12`'s LINEAR path is expected to work here, with Task 6's
  pitch check as the runtime guard.
- **many differ** → the surface is tiled. Say so, and the CPU copy is confirmed
  on evidence. Task 6 still gets built: it is the path for every *other*
  machine, and its rejection here is then a verified behaviour rather than an
  assumption.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-client-h264
git commit -m "test(h264): oracle -- hardware decode is bit-exact against software"
```

---

## Task 6: Import the dmabuf into wgpu

**Files:**
- Create: `ghostframe-client-gpu/src/import.rs`
- Modify: `ghostframe-client-gpu/src/lib.rs` (add `pub mod import;`), `ghostframe-client-gpu/Cargo.toml`

The mirror of `export.rs`, with one safeguard that matters: **wgpu-hal 30 exposes
`texture_from_raw` but no `buffer_from_raw`**, so the import must go through a
`VkImage`, and the driver — not us — chooses that image's `rowPitch`. Without
`VK_EXT_image_drm_format_modifier` (absent on this GPU) there is no way to *tell*
Vulkan the dmabuf's pitch. So the import creates the image, asks
`vkGetImageSubresourceLayout` what pitch it actually got, and refuses the import
if it disagrees with the descriptor. A mismatch would otherwise render as
skewed-but-plausible pixels, which is far harder to notice than a clean failure.

- [ ] **Step 1: Write the failing test**

The test builds its own linear dmabuf with the existing export path, writes a
known pattern into it, then imports it back as a single-channel texture. That
exercises the import machinery with no decoder involved.

```rust
// ghostframe-client-gpu/tests/gpu_import.rs
//! Import a dmabuf we created ourselves, and read it back.
//!
//! Requires a GPU. Deliberately NOT named in any CI workflow.

use ghostframe_client_gpu::export::ExportedImage;
use ghostframe_client_gpu::import::import_nv12;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_h264::{DmabufPlanes, PlaneDesc};

#[test]
fn imports_a_linear_dmabuf_and_reads_the_bytes_back() {
    let Ok(ctx) = WgpuContext::new() else {
        eprintln!("no usable GPU; skipping");
        return;
    };

    // 64x64 RGBA export = 16384 bytes, host-visible so we can write a pattern.
    let src = ExportedImage::new(&ctx, 64, 64, &[], true).expect("export");
    let pitch = src.planes[0].stride;

    // Treat the RGBA export as an NV12 luma plane of width `pitch`: the
    // import path only cares about bytes, offsets and pitches.
    let planes = DmabufPlanes {
        fd: src.raw_fd(),
        modifier: src.modifier,
        // The whole exported object. `from_descriptor` fills this from
        // `objects[0].size`; here we know it because we allocated it.
        size: pitch * 64,
        fourcc_luma: ghostframe_client_h264::DRM_FORMAT_R8,
        fourcc_chroma: ghostframe_client_h264::DRM_FORMAT_GR88,
        width: 64,
        height: 64,
        luma: PlaneDesc {
            offset: src.planes[0].offset,
            pitch,
        },
        chroma: PlaneDesc {
            offset: src.planes[0].offset,
            pitch,
        },
    };

    let imported = match import_nv12(&ctx, &planes) {
        Ok(i) => i,
        Err(e) => {
            panic!(
                "import of a LINEAR dmabuf this process just exported failed: {e}. \
                 This is the import path itself failing, not the decoder."
            );
        }
    };
    assert_eq!(imported.luma.width(), 64);
    assert_eq!(imported.chroma.width(), 32);
    assert_eq!(imported.chroma.height(), 32);
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_import
```

Expected: FAIL to compile — `import_nv12` does not exist.

- [ ] **Step 3: Implement the import**

```rust
// ghostframe-client-gpu/src/import.rs
//! Import an externally produced dmabuf as wgpu textures.
//!
//! The mirror of [`crate::export`], and subject to one constraint that
//! shapes everything here: **wgpu-hal 30 has `texture_from_raw` but no
//! `buffer_from_raw`**, so an imported dmabuf must become a `VkImage`. For a
//! `VK_IMAGE_TILING_LINEAR` image the driver picks the row pitch, and
//! without `VK_EXT_image_drm_format_modifier` -- which this hardware does
//! not expose -- there is no way to tell Vulkan the pitch the producer used.
//!
//! So the pitch is *checked*, not assumed: `vkGetImageSubresourceLayout`
//! reports what the driver chose, and a disagreement fails the import. A
//! wrong pitch does not produce obviously broken output; it produces a
//! sheared image that looks like a decode bug and costs a day.

use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ash::vk;
use ghostframe_client_h264::{DmabufPlanes, PlaneDesc};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// The two textures an NV12 dmabuf becomes.
pub struct ImportedNv12 {
    pub luma: wgpu::Texture,
    pub chroma: wgpu::Texture,
    images: Vec<vk::Image>,
    memory: vk::DeviceMemory,
    device: ash::Device,
    _fd: OwnedFd,
}

impl Drop for ImportedNv12 {
    fn drop(&mut self) {
        // SAFETY: images first, then the memory they were bound to -- freeing
        // memory under a live image is the invalid order. Both were created
        // here and are owned solely by this struct.
        unsafe {
            for image in self.images.drain(..) {
                self.device.destroy_image(image, None);
            }
            self.device.free_memory(self.memory, None);
        }
    }
}

/// Import `planes` as two single-plane textures: luma as `R8Unorm`, chroma as
/// `Rg8Unorm` at half resolution.
///
/// The fd is duplicated, so the caller keeps ownership of theirs and may drop
/// the mapped frame as soon as this returns.
pub fn import_nv12(ctx: &WgpuContext, planes: &DmabufPlanes) -> Result<ImportedNv12, GpuError> {
    // A tiled modifier cannot be imported without
    // VK_EXT_image_drm_format_modifier. Say so precisely rather than failing
    // deeper in with a confusing Vulkan error.
    const DRM_FORMAT_MOD_LINEAR: u64 = 0;
    if planes.modifier != DRM_FORMAT_MOD_LINEAR && !ctx.explicit_modifiers {
        return Err(GpuError::Vulkan(format!(
            "dmabuf has modifier 0x{:016x} but this adapter lacks \
             VK_EXT_image_drm_format_modifier, so only LINEAR (0) can be imported",
            planes.modifier
        )));
    }

    ctx.with_raw(|instance, device, phys| {
        import_inner(instance, device, phys, &ctx.device, planes)
    })
    .ok_or_else(|| GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string()))?
}

fn import_inner(
    instance: &ash::Instance,
    device: &ash::Device,
    phys: vk::PhysicalDevice,
    wgpu_device: &wgpu::Device,
    planes: &DmabufPlanes,
) -> Result<ImportedNv12, GpuError> {
    // Duplicate: vkImportMemoryFdKHR takes ownership of the fd it is given,
    // while the caller's `MappedFrame` still owns the original.
    // SAFETY: `planes.fd` is a live fd owned by the caller for the duration
    // of this call.
    let dup = unsafe { libc::dup(planes.fd) };
    if dup < 0 {
        return Err(GpuError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `dup` is a fresh fd this process now owns exclusively.
    let owned = unsafe { OwnedFd::from_raw_fd(dup) };

    let luma = create_linear_image(
        device,
        planes.width,
        planes.height,
        vk::Format::R8_UNORM,
    )?;
    let chroma = match create_linear_image(
        device,
        planes.chroma_width(),
        planes.chroma_height(),
        vk::Format::R8G8_UNORM,
    ) {
        Ok(i) => i,
        Err(e) => {
            // SAFETY: `luma` was created above and nothing else holds it.
            unsafe { device.destroy_image(luma, None) };
            return Err(e);
        }
    };

    let result = bind_and_wrap(
        instance, device, phys, wgpu_device, planes, luma, chroma, owned,
    );
    if result.is_err() {
        // SAFETY: on the error path neither image was handed to an
        // `ImportedNv12`, so this is the only owner.
        unsafe {
            device.destroy_image(chroma, None);
            device.destroy_image(luma, None);
        }
    }
    result
}

fn create_linear_image(
    device: &ash::Device,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<vk::Image, GpuError> {
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external);

    // SAFETY: `info` is fully populated and `device` is live.
    unsafe { device.create_image(&info, None) }
        .map_err(|e| GpuError::Vulkan(format!("create_image (import, {format:?}): {e}")))
}

#[allow(clippy::too_many_arguments)]
fn bind_and_wrap(
    instance: &ash::Instance,
    device: &ash::Device,
    phys: vk::PhysicalDevice,
    wgpu_device: &wgpu::Device,
    planes: &DmabufPlanes,
    luma: vk::Image,
    chroma: vk::Image,
    fd: OwnedFd,
) -> Result<ImportedNv12, GpuError> {
    let ext_mem_fd = ash::khr::external_memory_fd::Device::new(instance, device);

    // What memory types can back this fd?
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    // SAFETY: `fd` is a live dmabuf fd; `fd_props` is a valid out-param.
    unsafe {
        ext_mem_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd.as_raw_fd(),
            &mut fd_props,
        )
    }
    .map_err(|e| GpuError::Vulkan(format!("get_memory_fd_properties: {e}")))?;

    // SAFETY: both images are live.
    let luma_req = unsafe { device.get_image_memory_requirements(luma) };
    let chroma_req = unsafe { device.get_image_memory_requirements(chroma) };
    let type_bits = fd_props.memory_type_bits & luma_req.memory_type_bits & chroma_req.memory_type_bits;

    let mem_type_index = find_memory_type_index(instance, phys, type_bits).ok_or_else(|| {
        GpuError::Vulkan(
            "no memory type can back this dmabuf and both plane images".to_string(),
        )
    })?;

    // The chroma image binds at a nonzero offset into the same allocation,
    // which Vulkan only permits at a multiple of its alignment.
    if planes.chroma.offset % chroma_req.alignment != 0 {
        return Err(GpuError::Vulkan(format!(
            "chroma plane offset {} is not a multiple of the required alignment {}",
            planes.chroma.offset, chroma_req.alignment
        )));
    }

    // `planes.size` is what the driver reported for the dmabuf object.
    // Reconstructing it as `chroma.offset + chroma.pitch * chroma_height()`
    // looks equivalent and is not: any padding past the last chroma row, or an
    // alignment-driven taller chroma plane, makes the guess short, and a short
    // `vkAllocateMemory` surfaces as an opaque import failure with nothing
    // pointing back to the arithmetic.
    let total = planes.size;

    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(fd.as_raw_fd());
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(total)
        .memory_type_index(mem_type_index)
        .push_next(&mut import_info);

    // SAFETY: `alloc_info` imports exactly the fd above, sized to cover both
    // planes. On success Vulkan owns the fd, which is why `fd` is moved into
    // the returned struct and never closed separately.
    let memory = unsafe { device.allocate_memory(&alloc_info, None) }
        .map_err(|e| GpuError::Vulkan(format!("allocate_memory (import): {e}")))?;

    // SAFETY: images and memory are live and sized for each other.
    unsafe { device.bind_image_memory(luma, memory, planes.luma.offset) }.map_err(|e| {
        // SAFETY: nothing is bound yet, so freeing is sound.
        unsafe { device.free_memory(memory, None) };
        GpuError::Vulkan(format!("bind_image_memory (luma): {e}"))
    })?;
    // SAFETY: as above.
    unsafe { device.bind_image_memory(chroma, memory, planes.chroma.offset) }.map_err(|e| {
        // SAFETY: freeing memory with `luma` still bound is sound only
        // because `luma` is destroyed by the caller's error path before this
        // function's memory is reused; destroy it here first to keep the
        // documented order (images, then memory).
        unsafe {
            device.destroy_image(luma, None);
            device.free_memory(memory, None);
        }
        GpuError::Vulkan(format!("bind_image_memory (chroma): {e}"))
    })?;

    // THE CHECK. The driver chose these pitches; the producer chose the ones
    // in `planes`. If they differ, every row after the first reads from the
    // wrong offset and the image shears.
    check_pitch(device, luma, "luma", planes.luma)?;
    check_pitch(device, chroma, "chroma", planes.chroma)?;

    let luma_tex = wrap_texture(
        wgpu_device,
        luma,
        planes.width,
        planes.height,
        wgpu::TextureFormat::R8Unorm,
        "ghostframe-imported-luma",
    )?;
    let chroma_tex = wrap_texture(
        wgpu_device,
        chroma,
        planes.chroma_width(),
        planes.chroma_height(),
        wgpu::TextureFormat::Rg8Unorm,
        "ghostframe-imported-chroma",
    )?;

    Ok(ImportedNv12 {
        luma: luma_tex,
        chroma: chroma_tex,
        images: vec![luma, chroma],
        memory,
        device: device.clone(),
        _fd: fd,
    })
}

fn check_pitch(
    device: &ash::Device,
    image: vk::Image,
    what: &str,
    plane: PlaneDesc,
) -> Result<(), GpuError> {
    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        array_layer: 0,
    };
    // SAFETY: `image` is live, bound, and linear-tiled, which is the only
    // tiling for which this query is defined.
    let layout = unsafe { device.get_image_subresource_layout(image, subresource) };
    if layout.row_pitch != plane.pitch {
        return Err(GpuError::Vulkan(format!(
            "{what} plane pitch mismatch: the dmabuf says {}, the driver's linear \
             image wants {}. Importing anyway would shear the image. Use the CPU \
             copy path for this frame.",
            plane.pitch, layout.row_pitch
        )));
    }
    Ok(())
}

fn wrap_texture(
    wgpu_device: &wgpu::Device,
    image: vk::Image,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &'static str,
) -> Result<wgpu::Texture, GpuError> {
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    // SAFETY: `image` is a live VkImage bound to memory `ImportedNv12` owns.
    // The no-op drop callback keeps ownership here rather than letting
    // wgpu-hal destroy the image, and `TextureMemory::External` tells it the
    // memory is not its to free -- `ImportedNv12::drop` does both.
    // `UNINITIALIZED` is the image's true layout: it was just created.
    let hal_texture = unsafe {
        wgpu_device
            .as_hal::<wgpu_hal::api::Vulkan>()
            .map(|hal_device| {
                hal_device.texture_from_raw(
                    image,
                    &wgpu_hal::TextureDescriptor {
                        label: Some(label),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
                        memory_flags: wgpu_hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    },
                    Some(Box::new(|| {})),
                    wgpu_hal::vulkan::TextureMemory::External,
                )
            })
    }
    .ok_or_else(|| GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string()))?;

    // SAFETY: `hal_texture` was just built from a valid VkImage matching this
    // descriptor, and `UNINITIALIZED` reflects its true current layout.
    Ok(unsafe {
        wgpu_device.create_texture_from_hal::<wgpu_hal::api::Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        )
    })
}

fn find_memory_type_index(
    instance: &ash::Instance,
    phys: vk::PhysicalDevice,
    type_bits: u32,
) -> Option<u32> {
    // SAFETY: `phys` is a live physical device from `WgpuContext::with_raw`.
    let props = unsafe { instance.get_physical_device_memory_properties(phys) };
    (0..props.memory_type_count).find(|i| type_bits & (1 << i) != 0)
}
```

Add to `ghostframe-client-gpu/Cargo.toml` `[dependencies]`:

```toml
ghostframe-client-h264 = { path = "../ghostframe-client-h264" }
```

and `pub mod import;` to `ghostframe-client-gpu/src/lib.rs`.

- [ ] **Step 4: Run the test**

```bash
cargo test -p ghostframe-client-gpu --test gpu_import -- --nocapture
```

Expected: PASS. If it fails on the pitch check, that is the safeguard working —
record the two pitch values, they decide whether Task 9 uses the import path or
the CPU path on this hardware.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-gpu Cargo.toml Cargo.lock
git commit -m "feat(gpu): import an NV12 dmabuf as two textures, pitch-checked"
```

---

## Task 7: The shader and Oracle 2

**Files:**
- Create: `shaders/client/h264_nv12_blit.wgsl`, `ghostframe-client-gpu/src/nv12_reference.rs`
- Modify: `ghostframe-client-gpu/src/lib.rs`

- [ ] **Step 1: Write the Rust reference and its failing test**

```rust
// ghostframe-client-gpu/src/nv12_reference.rs
//! The CPU reference for `h264_nv12_blit.wgsl`.
//!
//! Exists so the shader can be checked against something, and so the
//! constants live in exactly two places that a test compares. Both invert
//! `ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp`, which is
//! **full-range BT.601** -- see the design doc §5. Not BT.709. Not limited
//! range.

/// Chroma is centred on 128/255, matching the forward shader's `+ 0.502`.
pub const CHROMA_CENTRE: f32 = 0.502;

/// Exact inverse of the forward matrix. The textbook full-range BT.601
/// inverse differs from this by at most 0.171/255 across the whole YUV cube,
/// so the difference is invisible -- these are used because inverting the
/// transform the encoder actually applied is the correct thing to do.
pub const R_COEFF: [f32; 3] = [1.0, -0.000927, 1.401687];
pub const G_COEFF: [f32; 3] = [1.0, -0.343695, -0.714169];
pub const B_COEFF: [f32; 3] = [1.0, 1.772160, 0.000990];

/// Convert one NV12 pixel to RGBA8.
///
/// `luma`/`chroma` are raw 8-bit samples. Chroma is upsampled
/// nearest-neighbour, not bilinear: the forward shader takes chroma from the
/// top-left pixel of each 2x2 block rather than averaging, so replicating
/// that sample is what inverts it.
pub fn nv12_pixel_to_rgba(luma: u8, cb: u8, cr: u8) -> [u8; 4] {
    let y = luma as f32 / 255.0;
    let u = cb as f32 / 255.0 - CHROMA_CENTRE;
    let v = cr as f32 / 255.0 - CHROMA_CENTRE;

    let r = R_COEFF[0] * y + R_COEFF[1] * u + R_COEFF[2] * v;
    let g = G_COEFF[0] * y + G_COEFF[1] * u + G_COEFF[2] * v;
    let b = B_COEFF[0] * y + B_COEFF[1] * u + B_COEFF[2] * v;

    [to_u8(r), to_u8(g), to_u8(b), 255]
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neutral chroma with full-range luma must round-trip to grey, at both
    /// ends. Under a LIMITED-range matrix, Y=0 would map to black only after
    /// a 16/255 pedestal subtraction and Y=255 would clip early -- so this
    /// test is what catches a well-meaning "fix" to BT.709 limited.
    #[test]
    fn full_range_luma_maps_to_the_full_grey_ramp() {
        assert_eq!(nv12_pixel_to_rgba(0, 128, 128), [0, 0, 0, 255]);
        assert_eq!(nv12_pixel_to_rgba(255, 128, 128), [255, 255, 255, 255]);
        let mid = nv12_pixel_to_rgba(128, 128, 128);
        assert!(
            (127..=129).contains(&mid[0]),
            "mid grey should stay mid grey, got {mid:?}"
        );
    }

    /// Inverting the forward shader on a known colour must return it. Red is
    /// the channel that the encoder-side R/B swap bug (fixed in 847d870) got
    /// wrong, so it is the one worth pinning.
    #[test]
    fn round_trips_pure_red_through_the_forward_matrix() {
        // Forward, from bgra_to_nv12.comp with R=1, G=0, B=0.
        let y = (0.299f32 * 255.0 + 0.5) as u8;
        let u = ((-0.169f32 + 0.502) * 255.0 + 0.5) as u8;
        let v = ((0.500f32 + 0.502) * 255.0 + 0.5) as u8;

        let rgba = nv12_pixel_to_rgba(y, u, v);
        assert!(rgba[0] > 250, "red channel should be ~255, got {rgba:?}");
        assert!(rgba[1] < 5, "green channel should be ~0, got {rgba:?}");
        assert!(rgba[2] < 5, "blue channel should be ~0, got {rgba:?}");
    }
}
```

- [ ] **Step 2: Run it**

```bash
cargo test -p ghostframe-client-gpu --lib nv12_reference
```

Expected: 2 passed, after adding `pub mod nv12_reference;` to `lib.rs`.

- [ ] **Step 3: Write the shader**

```wgsl
// shaders/client/h264_nv12_blit.wgsl
//
// NV12 -> RGBA, full-range BT.601.
//
// This is the exact inverse of ghostframe-lib/src/capture/shaders/
// bgra_to_nv12.comp, which is the ONLY reason these constants are what they
// are. The two files must change together; nothing else links them.
//
// NOT BT.709, and NOT limited range. The encoder writes full-range BT.601
// and signals no VUI colour description at all. An earlier design document
// claimed BT.709 limited; it was wrong.
//
// Chroma upsampling is nearest-neighbour (integer /2), not bilinear. The
// forward shader samples chroma at the top-left pixel of each 2x2 block
// instead of averaging, so replication is what inverts it. Bilinear would
// look smoother and be further from the source.
//
// Mirrored on the CPU in ghostframe-client-gpu/src/nv12_reference.rs, which
// tests/gpu_nv12_blit.rs asserts this against exactly.

@group(0) @binding(0) var luma_tex: texture_2d<f32>;
@group(0) @binding(1) var chroma_tex: texture_2d<f32>;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
  // Full-surface quad in NDC. The viewport covers the whole framebuffer:
  // H.264 always replaces the entire frame, never a tile rect.
  let xs = array<f32, 6>(-1.0,  1.0, -1.0,  1.0,  1.0, -1.0);
  let ys = array<f32, 6>(-1.0, -1.0,  1.0, -1.0,  1.0,  1.0);
  var out: VsOut;
  out.pos = vec4<f32>(xs[vi], ys[vi], 0.0, 1.0);
  return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
  // textureLoad, not textureSample: integer coordinates with no sampler make
  // the nearest-neighbour chroma rule explicit and unfilterable.
  let p = vec2<i32>(i32(in.pos.x), i32(in.pos.y));
  let y = textureLoad(luma_tex, p, 0).r;
  let c = textureLoad(chroma_tex, p / 2, 0).rg;

  let u = c.r - 0.502;
  let v = c.g - 0.502;

  let r = y - 0.000927 * u + 1.401687 * v;
  let g = y - 0.343695 * u - 0.714169 * v;
  let b = y + 1.772160 * u + 0.000990 * v;

  return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
}
```

- [ ] **Step 4: Commit the shader and reference**

```bash
git add shaders/client/h264_nv12_blit.wgsl ghostframe-client-gpu/src/nv12_reference.rs ghostframe-client-gpu/src/lib.rs
git commit -m "feat(gpu): NV12 -> RGBA shader and its CPU reference, full-range BT.601"
```

---

## Task 8: The pipeline, and Oracle 2 against the shader

**Files:**
- Create: `ghostframe-client-gpu/src/pipelines/h264_nv12.rs`, `ghostframe-client-gpu/tests/gpu_nv12_blit.rs`
- Modify: `ghostframe-client-gpu/src/pipelines/mod.rs`

- [ ] **Step 1: Write the failing oracle test**

```rust
// ghostframe-client-gpu/tests/gpu_nv12_blit.rs
//! Oracle: the NV12 shader against its CPU reference, exactly.
//!
//! Requires a GPU. Deliberately NOT named in any CI workflow.

use ghostframe_client_gpu::framebuffer::Framebuffer;
use ghostframe_client_gpu::nv12_reference::nv12_pixel_to_rgba;
use ghostframe_client_gpu::pipelines::h264_nv12::H264Nv12Pipeline;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;

const W: u32 = 64;
const H: u32 = 64;

/// Synthetic NV12 that sweeps luma across the full 0..255 range and walks
/// chroma independently, so a limited-range matrix cannot pass by accident.
fn synthetic_nv12() -> (Vec<u8>, Vec<u8>) {
    let mut luma = vec![0u8; (W * H) as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            luma[y * W as usize + x] = ((x * 4 + y) % 256) as u8;
        }
    }
    let mut chroma = vec![0u8; (W * H / 2) as usize];
    for y in 0..(H / 2) as usize {
        for x in 0..(W / 2) as usize {
            let i = (y * (W / 2) as usize + x) * 2;
            chroma[i] = ((x * 8) % 256) as u8;
            chroma[i + 1] = ((y * 8 + 64) % 256) as u8;
        }
    }
    (luma, chroma)
}

#[test]
fn the_shader_matches_the_cpu_reference_exactly() {
    let Ok(ctx) = WgpuContext::new() else {
        eprintln!("no usable GPU; skipping");
        return;
    };

    let (luma, chroma) = synthetic_nv12();
    let mut pipeline = H264Nv12Pipeline::new(&ctx.device);
    let mut fb = Framebuffer::new(&ctx.device, W, H);

    let (luma_tex, chroma_tex) =
        H264Nv12Pipeline::upload_planes(&ctx.device, &ctx.queue, W, H, &luma, &chroma);
    pipeline.draw(&ctx.device, &ctx.queue, &fb, &luma_tex, &chroma_tex);

    let got = fb.debug_read(&ctx.device, &ctx.queue);

    let mut mismatches = Vec::new();
    for y in 0..H as usize {
        for x in 0..W as usize {
            let cx = x / 2;
            let cy = y / 2;
            let ci = (cy * (W / 2) as usize + cx) * 2;
            let want = nv12_pixel_to_rgba(luma[y * W as usize + x], chroma[ci], chroma[ci + 1]);
            let i = (y * W as usize + x) * 4;
            let have = [got[i], got[i + 1], got[i + 2], got[i + 3]];
            if have != want {
                mismatches.push((x, y, want, have));
            }
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} of {} pixels differ between the shader and the CPU reference. \
         First 5: {:?}. Both implement the same full-range BT.601 inverse, so \
         a difference is a real bug -- investigate before considering any \
         tolerance, and if f32 rounding really is the cause, record how many \
         pixels and at which boundary.",
        mismatches.len(),
        W * H,
        &mismatches[..mismatches.len().min(5)]
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_nv12_blit
```

Expected: FAIL to compile — `H264Nv12Pipeline` does not exist.

- [ ] **Step 3: Implement the pipeline**

```rust
// ghostframe-client-gpu/src/pipelines/h264_nv12.rs
//! Full-frame NV12 -> RGBA blit into the framebuffer.
//!
//! Unlike the tile pipelines, this one always covers the whole surface:
//! H.264 frame mode replaces the entire image, so there is no instancing and
//! no tile rect.

use crate::framebuffer::Framebuffer;

const SHADER_SRC: &str = include_str!("../../../shaders/client/h264_nv12_blit.wgsl");

pub struct H264Nv12Pipeline {
    pipeline: wgpu::RenderPipeline,
}

impl H264Nv12Pipeline {
    pub fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ghostframe-h264-nv12-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ghostframe-h264-nv12-pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        H264Nv12Pipeline { pipeline }
    }

    /// Upload CPU-side NV12 planes as textures. Used by the oracle, and by
    /// the CPU fallback path when a dmabuf cannot be imported.
    pub fn upload_planes(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        luma: &[u8],
        chroma: &[u8],
    ) -> (wgpu::Texture, wgpu::Texture) {
        let luma_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ghostframe-h264-luma"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &luma_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            luma,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let chroma_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ghostframe-h264-chroma"),
            size: wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &chroma_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            chroma,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height / 2),
            },
            wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
        );

        (luma_tex, chroma_tex)
    }

    /// Draw the whole frame into `fb`.
    ///
    /// `LoadOp::Load`, never `Clear`: the framebuffer is shared with the tile
    /// codecs, and the draw covers every pixel anyway.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fb: &Framebuffer,
        luma: &wgpu::Texture,
        chroma: &wgpu::Texture,
    ) {
        let luma_view = luma.create_view(&wgpu::TextureViewDescriptor::default());
        let chroma_view = chroma.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ghostframe-h264-nv12-bind-group"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&chroma_view),
                },
            ],
        });

        let fb_view = fb
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-h264-nv12-encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ghostframe-h264-nv12-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &fb_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        queue.submit(Some(encoder.finish()));
    }
}
```

Add `pub mod h264_nv12;` to `ghostframe-client-gpu/src/pipelines/mod.rs`.

- [ ] **Step 4: Run the oracle**

```bash
cargo test -p ghostframe-client-gpu --test gpu_nv12_blit -- --nocapture
```

Expected: `test result: ok. 1 passed`.

If pixels differ, read the mismatch list before changing anything. A handful of
pixels off by one at a `.5` rounding boundary is an f32 story worth measuring and
recording. Whole regions wrong is a coordinate or pitch bug. The two need
opposite fixes.

- [ ] **Step 5: Prove the oracle is load-bearing**

Temporarily change the shader's `1.401687` to `1.402` (the textbook constant),
re-run, and confirm it FAILS. Revert. This proves the test actually pins the
constants rather than passing on a coincidence.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-gpu
git commit -m "feat(gpu): NV12 blit pipeline, oracle-checked against the CPU reference"
```

---

## Task 9: Wire it into the renderer

**Files:**
- Modify: `ghostframe-client-gpu/src/renderer.rs`

- [ ] **Step 1: Write the failing test**

```rust
// ghostframe-client-gpu/tests/gpu_h264_render.rs
//! An access unit in, correct pixels in the framebuffer out.
//!
//! Requires a GPU and VA-API. Deliberately NOT named in any CI workflow.

use ghostframe_client_core::Event;
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_h264::testclip::gradient_clip;

#[test]
fn a_decoded_frame_lands_in_the_framebuffer() {
    let Ok(ctx) = WgpuContext::new() else {
        eprintln!("no usable GPU; skipping");
        return;
    };
    if !ghostframe_client_h264::vaapi_h264_decode_available() {
        eprintln!("no VA-API H.264 decode here; skipping");
        return;
    }

    let mut renderer = Renderer::new(&ctx, 640, 480, 3, &[], true).expect("renderer");
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: 640,
            height: 480,
        },
    );

    let clip = gradient_clip(640, 480, 4);
    for (i, au) in clip.iter().enumerate() {
        renderer.apply_event(
            &ctx,
            &Event::NeedsH264 {
                frame_seq: i as u32,
                timestamp_us: i as u32 * 16_667,
                is_keyframe: i == 0,
                payload: au.clone(),
            },
        );
    }
    renderer.flush(&ctx);

    let pixels = renderer.debug_read_framebuffer(&ctx);
    assert_eq!(pixels.len(), 640 * 480 * 4);

    // The clip is a gradient, so a framebuffer that stayed black means
    // nothing was drawn at all.
    let nonblack = pixels.chunks_exact(4).filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8).count();
    assert!(
        nonblack > 640 * 480 / 4,
        "only {nonblack} of {} pixels are non-black -- the decode path drew nothing",
        640 * 480
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_h264_render
```

Expected: FAIL — the framebuffer stays black, because `NeedsH264` is still a no-op.

- [ ] **Step 3: Replace the no-op arm**

In `ghostframe-client-gpu/src/renderer.rs`, add fields to `Renderer`:

```rust
    /// Lazily created: a client that never receives H.264 never opens a
    /// decoder, and a machine without VA-API never can.
    h264_decoder: Option<ghostframe_client_h264::decoder::H264Decoder>,
    h264_pipeline: Option<crate::pipelines::h264_nv12::H264Nv12Pipeline>,
    /// Logged once, so the import path taken is visible without spamming
    /// every frame. `None` until the first H.264 frame decides it.
    h264_path_logged: bool,
```

Initialise all three (`None`, `None`, `false`) in `Renderer::new`, then replace
the `Event::NeedsH264 { .. } => { ... }` arm with:

```rust
            Event::NeedsH264 {
                frame_seq,
                payload,
                is_keyframe,
                ..
            } => {
                self.decode_h264(ctx, *frame_seq, *is_keyframe, payload);
            }
```

and add these methods to `impl Renderer`:

```rust
    /// Decode one access unit and blit every frame it produced.
    ///
    /// Failures are logged, never fatal: a corrupt access unit (FEC failed to
    /// recover one) must not take down the session, and the next keyframe
    /// recovers.
    fn decode_h264(&mut self, ctx: &WgpuContext, frame_seq: u32, is_keyframe: bool, au: &[u8]) {
        if self.h264_decoder.is_none() {
            match ghostframe_client_h264::decoder::H264Decoder::new() {
                Ok(d) => self.h264_decoder = Some(d),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "H.264 arrived but no decoder could be opened -- the capability \
                         was advertised without hardware to back it"
                    );
                    return;
                }
            }
        }
        if self.h264_pipeline.is_none() {
            self.h264_pipeline = Some(crate::pipelines::h264_nv12::H264Nv12Pipeline::new(
                &ctx.device,
            ));
        }

        let decoder = self.h264_decoder.as_mut().expect("just created");
        let frames = match decoder.decode(au) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(frame_seq, is_keyframe, error = %e, "H.264 decode failed");
                return;
            }
        };

        for frame in frames {
            self.blit_h264_frame(ctx, frame_seq, &frame);
        }
    }

    fn blit_h264_frame(
        &mut self,
        ctx: &WgpuContext,
        frame_seq: u32,
        frame: &ghostframe_client_h264::decoder::HwFrame,
    ) {
        // A decoded size that disagrees with the framebuffer means the tile
        // stream's FrameDimensions and the H.264 SPS disagree. Blitting
        // anyway corrupts the framebuffer; dropping recovers at the next
        // keyframe.
        if frame.width() != self.fb.width || frame.height() != self.fb.height {
            tracing::warn!(
                frame_seq,
                decoded_w = frame.width(),
                decoded_h = frame.height(),
                fb_w = self.fb.width,
                fb_h = self.fb.height,
                "dropping H.264 frame whose size disagrees with the framebuffer"
            );
            return;
        }

        let pipeline = self.h264_pipeline.as_mut().expect("created by caller");

        // Preferred path: import the decoder's dmabuf directly. Falls back to
        // a CPU download when the surface is tiled, misaligned, or the
        // driver's linear pitch disagrees with the producer's -- all of which
        // `import_nv12` reports rather than rendering wrong pixels.
        let imported = frame
            .map_dmabuf()
            .and_then(|mapped| {
                crate::import::import_nv12(ctx, mapped.planes())
                    .map_err(|e| ghostframe_client_h264::H264Error::Ffmpeg(e.to_string()))
                    .map(|imported| (mapped, imported))
            });

        match imported {
            Ok((_mapped, imported)) => {
                if !self.h264_path_logged {
                    tracing::info!("H.264 import path: zero-copy dmabuf");
                    self.h264_path_logged = true;
                }
                pipeline.draw(
                    &ctx.device,
                    &ctx.queue,
                    &self.fb,
                    &imported.luma,
                    &imported.chroma,
                );
            }
            Err(e) => {
                if !self.h264_path_logged {
                    tracing::info!(reason = %e, "H.264 import path: CPU copy (dmabuf import unavailable)");
                    self.h264_path_logged = true;
                }
                let Some((luma, chroma)) = frame.download_nv12() else {
                    tracing::warn!(frame_seq, "H.264 CPU download failed");
                    return;
                };
                let (luma_tex, chroma_tex) =
                    crate::pipelines::h264_nv12::H264Nv12Pipeline::upload_planes(
                        &ctx.device,
                        &ctx.queue,
                        self.fb.width,
                        self.fb.height,
                        &luma,
                        &chroma,
                    );
                pipeline.draw(&ctx.device, &ctx.queue, &self.fb, &luma_tex, &chroma_tex);
            }
        }

        // H.264 replaces the whole frame, so every tile is dirty.
        self.ring.mark_dirty_all();
    }
```

- [ ] **Step 4: Set the decoder up for per-frame use on the render thread**

Task 3's review surfaced two things that only bite once `decode()` runs per
frame from the render loop. One is cheap to set at construction; the other is a
constraint to record where it would be broken.

In `H264Decoder::with_device`, before `avcodec_open2`:

```rust
            // Without LOW_DELAY, a stream whose SPS carries a non-zero
            // max_num_reorder_frames makes the decoder hold every frame for one
            // frame-time before emitting it. That is invisible to a test that
            // counts total frames, and very visible to someone watching a
            // remote desktop. The server encodes without B-frames, so there is
            // nothing to reorder and nothing to lose here.
            // SAFETY: `ctx` is an allocated, not-yet-opened codec context.
            unsafe { (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32 };
```

On the frames pool: each `HwFrame` holds a VA-API surface out of the decoder's
pool for as long as it lives. `blit_h264_frame` below drops every frame within
the loop iteration, so nothing is held across presents and the default pool
size is fine — but that is a property of this code, not a guarantee. A later
change that queues frames or hands them to the compositor makes `receive_frame`
stall on pool exhaustion rather than fail, which reads as a hang. Say so in
`blit_h264_frame`'s doc comment, where someone making that change will see it.

- [ ] **Step 5: Add the CPU download to `HwFrame`**

In `ghostframe-client-h264/src/decoder.rs`, add to `impl HwFrame`:

```rust
    /// Download this surface to system memory as tightly packed NV12.
    ///
    /// The fallback when the dmabuf cannot be imported (tiled modifier,
    /// misaligned plane offset, or a driver row pitch that disagrees with
    /// the producer's). Costs a full-frame copy across the bus; correct
    /// everywhere.
    pub fn download_nv12(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        let w = self.width() as usize;
        let h = self.height() as usize;
        // SAFETY: `self.frame` is a live VAAPI frame; `sw` is freed on every
        // path out of this function.
        unsafe {
            let sw = ffi::av_frame_alloc();
            if sw.is_null() {
                return None;
            }
            (*sw).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
            if ffi::av_hwframe_transfer_data(sw, self.frame, 0) < 0 {
                ffi::av_frame_free(&mut { sw });
                return None;
            }
            let y_stride = (*sw).linesize[0] as usize;
            let uv_stride = (*sw).linesize[1] as usize;
            let mut luma = Vec::with_capacity(w * h);
            for row in 0..h {
                luma.extend_from_slice(std::slice::from_raw_parts(
                    (*sw).data[0].add(row * y_stride),
                    w,
                ));
            }
            let mut chroma = Vec::with_capacity(w * h / 2);
            for row in 0..h / 2 {
                chroma.extend_from_slice(std::slice::from_raw_parts(
                    (*sw).data[1].add(row * uv_stride),
                    w,
                ));
            }
            ffi::av_frame_free(&mut { sw });
            Some((luma, chroma))
        }
    }
```

- [ ] **Step 6: Run the test**

```bash
cargo test -p ghostframe-client-gpu --test gpu_h264_render -- --nocapture 2>&1 | grep -E 'import path|test result'
```

Expected: PASS, and one `H.264 import path: ...` line naming which path ran.
**Record which one** — it is the answer Task 1 predicted, now confirmed end to end.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-client-gpu ghostframe-client-h264
git commit -m "feat(gpu): decode and blit H.264 frames, with a CPU fallback path"
```

---

## Task 10: Flip the capability

**Files:**
- Modify: `ghostframe-client-native/src/lib.rs`, `ghostframe-client-native/Cargo.toml`, `ghostframe-client-capi/src/lib.rs`, `ghostframe-cli/src/commands.rs`, `ghostframe-e2e/tests/showcase.rs`

- [ ] **Step 1: Write the failing test**

```rust
// ghostframe-client-native/tests/h264_capability.rs
//! The advertised capability is the AND of what the host asked for and what
//! the machine can do.

use ghostframe_client_native::{Client, Config};

fn config(supports_h264: bool) -> Config {
    Config {
        hostname: "cap-test".into(),
        authkey: String::new(),
        state_dir: std::env::temp_dir().join("ghostframe-cap-test"),
        supports_h264,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        debug_map_frames: false,
    }
}

/// A host that does not want H.264 never gets it, regardless of hardware.
#[test]
fn opting_out_is_absolute() {
    let client = Client::new(config(false)).expect("create");
    assert!(!client.supports_h264());
}

/// A host that asks for H.264 gets it only where the hardware agrees. On a
/// machine with VA-API this is `true`; on one without, `false` -- and the
/// client is still perfectly usable, which is the point.
#[test]
fn opting_in_follows_the_probe() {
    let client = Client::new(config(true)).expect("create");
    assert_eq!(
        client.supports_h264(),
        ghostframe_client_h264::vaapi_h264_decode_available(),
        "the effective capability must equal the probe when the host opts in"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-native --test h264_capability
```

Expected: FAIL to compile — `supports_h264()` does not exist.

- [ ] **Step 3: Implement the probe wiring**

Add `ghostframe-client-h264 = { path = "../ghostframe-client-h264" }` to
`ghostframe-client-native/Cargo.toml`.

In `ghostframe-client-native/src/lib.rs`, add a field to `Client`:

```rust
    /// The capability actually advertised in HELLO: what the host asked for,
    /// AND what the machine can do. Computed once in `new`, because HELLO is
    /// built at connect and never revised.
    effective_h264: bool,
```

In `Client::new`, before constructing `Self`:

```rust
        let effective_h264 = config.supports_h264 && {
            let probed = ghostframe_client_h264::vaapi_h264_decode_available();
            if config.supports_h264 && !probed {
                tracing::info!(
                    "H.264 was requested but VA-API decode is unavailable here; \
                     advertising tile codecs only"
                );
            }
            probed
        };
```

then add `effective_h264,` to the struct literal, and this accessor:

```rust
    /// The H.264 capability this client actually advertises.
    ///
    /// `Config::supports_h264` is a *permission*, not an assertion: a host
    /// may ask for H.264 on a machine that cannot decode it, and gets a
    /// working session on the tile codecs rather than a failure.
    pub fn supports_h264(&self) -> bool {
        self.effective_h264
    }
```

Finally, change the `ClientConfig` construction at `ghostframe-client-native/src/lib.rs:295`
from `supports_h264: self.config.supports_h264` to `supports_h264: self.effective_h264`.

- [ ] **Step 4: Add the C ABI getter**

In `ghostframe-client-capi/src/lib.rs`, next to `gf_client_event_fd`:

```rust
/// The H.264 capability the client actually advertised.
///
/// `gf_client_config.supports_h264` is a request; this is the answer. A host
/// that asked for H.264 on a machine without VA-API gets `false` here and a
/// working session on the tile codecs.
///
/// Returns `false` for a null client.
#[no_mangle]
pub unsafe extern "C" fn gf_client_supports_h264(c: *const gf_client) -> bool {
    if c.is_null() {
        return false;
    }
    // SAFETY: `c` is non-null and, per this API's contract, points at a live
    // client created by `gf_client_create`.
    let client = unsafe { &*(c as *const crate::ClientHandle) };
    client.inner.supports_h264()
}
```

Match the exact handle-deref pattern `gf_client_event_fd` uses; if it differs
from the above, follow the existing one rather than this sketch.

- [ ] **Step 5: Flip the CLI and pin the showcase test**

In `ghostframe-cli/src/commands.rs:249`, change `supports_h264: false` to
`supports_h264: true`.

In `ghostframe-e2e/tests/showcase.rs`, add `("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "tile")`
to the `extra_env` of its `E2eServerSpec`, with this comment above it:

```rust
        // Pin tile mode. Sessions start in H.264 (`io_bridge.rs:3435`), so
        // once the CLI advertises the capability this test would silently
        // become an H.264 test instead of the tile-path test it was written
        // as. That is covered by `tests/h264.rs`; this one stays what it is.
```

- [ ] **Step 6: Regenerate and check the header**

```bash
cargo build -p ghostframe-client-capi
grep -n "gf_client_supports_h264" ghostframe-client-capi/include/ghostframe_client.h
```

Expected: the generated declaration appears.

- [ ] **Step 7: Run the tests**

```bash
cargo test -p ghostframe-client-native --test h264_capability
cargo test -p ghostframe-cli
cargo test -p ghostframe-client-capi
```

Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-client-native ghostframe-client-capi ghostframe-cli ghostframe-e2e/tests/showcase.rs Cargo.lock
git commit -m "feat(client): probe VA-API and advertise H.264 when it is real"
```

---

## Task 11: e2e acceptance against a live server

**Files:**
- Create: `ghostframe-e2e/tests/h264.rs`
- Modify: `ghostframe-e2e/Cargo.toml` (if `ghostframe-client-h264` is not already reachable)

**The server's H.264 path needs the GPU capture pipeline.** `io_bridge.rs` feeds
the full-frame encoder from `analysis.nv12_data`, which comes from
`capture/gpu_pipeline`. So this test uses `gpu: true` and `--drm-direct`, like
`e2e_mode_switch_chromium` — *not* the CPU capture path `native_client.rs` uses.
Requires the host VKMS setup.

- [ ] **Step 1: Write the test**

```rust
// ghostframe-e2e/tests/h264.rs
//! M3 acceptance: H.264 frames from a live server, decoded on the GPU.
//!
//! Requires Docker, a GPU, VA-API, and the host VKMS setup (the server's
//! full-frame H.264 encoder is fed by the GPU capture pipeline, so the CPU
//! capture path produces no H.264 at all). Deliberately NOT named in any CI
//! workflow.
//!
//! Run as:
//! ```text
//! TS_CONTROL_URL=http://127.0.0.1:18080 \
//!   cargo test -p ghostframe-e2e --test h264 -- --nocapture --test-threads=1
//! ```
//! `TS_CONTROL_URL` must be in the PROCESS environment: ghostbridge's Go
//! `init()` reads it before `main` to choose the DERP transport.

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, ClientEvent, Config, PublishedFrame};
use ghostframe_e2e::harness::{setup_e2e_server, E2eServerSpec};

/// Pump events until a frame is published, or `timeout` elapses. Copied from
/// `native_client.rs:55`: `Client` exposes no blocking frame call, and an
/// `acquire_frame` poll that never drains `next_event` would swallow the very
/// error events that explain a stall.
fn wait_for_frame(client: &mut Client, timeout: Duration) -> Option<PublishedFrame> {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(ev) = client.next_event() {
            tracing::info!(?ev, "client event");
            if let ClientEvent::Error { message } = &ev {
                panic!("client reported an error while waiting for a frame: {message}");
            }
        }
        if let Some(frame) = client.acquire_frame() {
            return Some(frame);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn h264_frames_from_the_server_render_into_the_dmabuf() {
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--gradient --drm-direct",
        // Pin H.264 rather than relying on the adaptation policy choosing it.
        // Sessions already start in H.264 (`io_bridge.rs:3435`), but "already"
        // is how a test comes to depend on a default nobody meant it to.
        extra_env: &[("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "h264")],
        // The server's full-frame H.264 encoder reads NV12 from the GPU
        // capture pipeline; with CPU capture there is no H.264 to receive.
        gpu: true,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = Client::new(Config {
        hostname: "h264-client-test".into(),
        authkey: String::new(),
        state_dir: state_dir.path().to_path_buf(),
        supports_h264: true,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        debug_map_frames: true,
    })
    .expect("create client");

    assert!(
        client.supports_h264(),
        "this machine must have VA-API H.264 decode to run the M3 acceptance test"
    );

    // Share the harness's tsnet node: a second tsnet.Server in one process
    // does not converge a working peer datapath.
    client.attach_bridge(setup._test_node.bridge());
    client
        .connect(&setup.server_container_name, 443)
        .expect("connect over tsnet");

    let frame = wait_for_frame(&mut client, Duration::from_secs(60))
        .expect("no frame published within 60s -- check the server entered H.264 mode");

    let f = client.debug_map_frame(&frame).expect("map exported dmabuf");

    // Rows are stride-padded, NOT tightly packed: `DebugFrameBytes` carries
    // `offset` and `stride` precisely because the export buffer's row pitch is
    // the driver's choice. Indexing as `y * width * 4` reads the wrong pixels
    // on any padded surface, and fails in a way that looks like a decode bug.
    let sample = |x: u64, y: u64| -> [u8; 4] {
        let o = (f.offset + y * f.stride + x * 4) as usize;
        [f.bytes[o], f.bytes[o + 1], f.bytes[o + 2], f.bytes[o + 3]]
    };

    // H.264 is lossy, so this asserts structure rather than exact values: a
    // gradient must actually vary across the frame. The exact-value gates are
    // the two oracles (hw == sw decode, shader == reference), which is where
    // exactness belongs and where a tolerance would do real damage.
    let mut lo = 255u8;
    let mut hi = 0u8;
    for y in (0..u64::from(frame.height)).step_by(16) {
        for x in (0..u64::from(frame.width)).step_by(16) {
            let px = sample(x, y);
            lo = lo.min(px[0]);
            hi = hi.max(px[0]);
        }
    }

    eprintln!("[m3] red channel spans {lo}..{hi} across the decoded frame");
    assert!(
        hi - lo > 32,
        "decoded red channel spans only {lo}..{hi}. The test pattern is a gradient, \
         so a near-flat frame means the decode or the colour conversion failed -- \
         not ordinary H.264 quantisation. A frame that never arrived at all fails \
         earlier, in wait_for_frame."
    );

    client.release_frame(frame.frame_id);
    client.disconnect().expect("disconnect");
}
```

- [ ] **Step 2: Confirm the borrowed helper still matches**

`wait_for_frame` above is copied from `ghostframe-e2e/tests/native_client.rs:55`.
Diff the two and take that file's version if it has moved on. The calls it uses
(`next_event`, `acquire_frame`, `debug_map_frame`, `release_frame`) are the whole
frame-side surface `Client` has; there is no blocking frame API to reach for.

- [ ] **Step 3: Rebuild the container image**

`cargo test` does NOT rebuild the test-server image, so a stale binary would run
silently:

```bash
just containers-build
```

- [ ] **Step 4: Run the test**

```bash
TS_CONTROL_URL=http://127.0.0.1:18080 \
  cargo test -p ghostframe-e2e --test h264 -- --nocapture --test-threads=1 2>&1 | tail -30
```

Expected: PASS, with the `[m3]` spread line printed.

If it times out, check in this order: (1) did the server actually enter H.264
mode — grep the container logs for `h264`; (2) did the client advertise the
capability — the assert above covers it; (3) is the VKMS host setup live.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-e2e/tests/h264.rs
git commit -m "test(e2e): M3 acceptance -- H.264 from a live server into the dmabuf"
```

---

## Task 12: Measure

**Files:**
- Modify: `ghostframe-client-gpu/src/renderer.rs`, `docs/superpowers/specs/2026-09-23-native-client-m3-design.md`

M2's precedent: measure, then decide, and write the numbers down. A deferral
backed by a number is a decision; one backed by a guess is a bet.

- [ ] **Step 1: Add the timing**

In `Renderer::decode_h264`, wrap the `decoder.decode(au)` call:

```rust
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware decode stall for M3 §10, not a virtual-clock path"
        )]
        let decode_start = std::time::Instant::now();
        let frames = match decoder.decode(au) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(frame_seq, is_keyframe, error = %e, "H.264 decode failed");
                return;
            }
        };
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware decode stall for M3 §10, not a virtual-clock path"
        )]
        let decode_us = decode_start.elapsed().as_micros() as u64;
        tracing::debug!(decode_us, frames = frames.len(), "h264: decode");
```

- [ ] **Step 2: Collect a run**

```bash
TS_CONTROL_URL=http://127.0.0.1:18080 \
  RUST_LOG=ghostframe_client_gpu::renderer=debug,ghostframe_client_gpu::ring=debug \
  cargo test -p ghostframe-e2e --test h264 -- --nocapture --test-threads=1 2>&1 \
  | grep -oE 'decode_us=[0-9]+|poll_us=[0-9]+' > /tmp/claude-1000/-home-cedric-work-ghostframe/eebfbb06-980b-4e7b-be07-0cdbcc8f6008/scratchpad/m3-timings.txt
wc -l < /tmp/claude-1000/-home-cedric-work-ghostframe/eebfbb06-980b-4e7b-be07-0cdbcc8f6008/scratchpad/m3-timings.txt
```

- [ ] **Step 3: Compute the percentiles**

```bash
python3 - <<'PY'
import re, statistics
vals = {"decode_us": [], "poll_us": []}
for line in open("/tmp/claude-1000/-home-cedric-work-ghostframe/eebfbb06-980b-4e7b-be07-0cdbcc8f6008/scratchpad/m3-timings.txt"):
    k, v = line.strip().split("=")
    vals[k].append(int(v))
for k, xs in vals.items():
    if not xs:
        print(f"{k}: no samples"); continue
    xs.sort()
    p = lambda q: xs[min(len(xs)-1, int(len(xs)*q))]
    print(f"{k}: n={len(xs)} p50={p(0.5)} p99={p(0.99)} max={xs[-1]}")
PY
```

- [ ] **Step 4: Write the numbers into the spec**

Replace §10's bullet list in
`docs/superpowers/specs/2026-09-23-native-client-m3-design.md` with the measured
values: decode p50/p99/max, the import path that ran, and whether `poll_us`
moved against M2's recorded p50 97µs / p99 624µs. State the caveats plainly —
one run, one GPU, one clip — and say explicitly whether a decode thread is now
justified, with the number that justifies it either way.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-gpu/src/renderer.rs docs/superpowers/specs/2026-09-23-native-client-m3-design.md
git commit -m "task(m3-12): measure H.264 decode cost and record the numbers"
```

---

## Task 13: CI and the local gate

**Files:**
- Modify: `.github/workflows/e2e.yml`

The new crate's pure-logic tests must run in CI; its GPU/VA-API tests must not
be named anywhere, because runners have neither. A new `tests/*.rs` is invisible
to CI until a workflow lists it.

- [ ] **Step 1: Name the crate's CI-visible tests**

In `.github/workflows/e2e.yml`, after the `cargo test -p ghostframe-cli` line
added in M2:

```yaml
      # ghostframe-client-h264's descriptor tests are pure logic and run
      # anywhere: they build their structs by hand. Its GPU/VA-API targets
      # (oracle_decode) are deliberately NOT named -- runners have neither, and
      # an #[ignore] would hide them on developer machines too.
      #
      # Note what this line does NOT cover. The decoder and probe tests gate
      # themselves on `vainfo` reporting a VAProfileH264*/VAEntrypointVLD pair.
      # Runners have neither libva-utils nor a VA-API device, so those tests
      # SKIP here and only ever really run on a developer machine. That is a
      # capability gate rather than a CI carve-out, but the effect is the same
      # and it should be visible rather than discovered: nothing in CI fails if
      # VA-API decode breaks.
      - run: cargo test -p ghostframe-client-h264 --lib
```

- [ ] **Step 2: Confirm the ffmpeg dev libraries are already installed in CI**

The new crate links libavcodec. `.github/workflows/_setup-system-deps/action.yml`
already installs `libavcodec-dev`, `libavformat-dev`, `libavutil-dev`,
`libswscale-dev` and `libavdevice-dev`. Confirm by reading the file — M2 went red
on exactly this class of gap, where the dev box had a library the runner did not.
If anything the new crate needs is missing, add it there, and verify the package
name in a throwaway `docker run --rm ubuntu:24.04` rather than guessing.

- [ ] **Step 3: Run the full local gate**

```bash
just ci-local
```

Expected: every section passes. This runs fmt, clippy `-D warnings`, the
workspace lib tests, the web build, the release build, the cbindgen header check
and the Go checks.

- [ ] **Step 4: Verify the container image still builds**

```bash
just containers-build 2>&1 | tail -5
```

Expected: success. A new workspace member whose manifest the Dockerfile does not
copy fails here (Task 2 added it; this confirms).

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/e2e.yml
git commit -m "ci: run ghostframe-client-h264's pure-logic tests"
```

---

## Not built, deliberately

**The VA-API VPP-to-linear path.** The design's §7 lists three outcomes for a
tiled decode surface: direct import, a VPP pass into a linear surface, or a CPU
copy. This plan builds the first and the third. VPP is skipped because forcing a
*linear* VPP target needs surface attributes ffmpeg's filter API does not expose
— so it means a direct libva dependency and a second FFI surface, bought for an
optimisation whose value is unknown until Task 12 measures the copy it would
replace. If those numbers show the copy costs real frames, VPP is the next thing
to build, and the measurement is what justifies it.

---

## Done means

- [ ] Task 1's modifier finding is recorded in the spec, and Task 9's log line agrees with it.
- [ ] `cargo test -p ghostframe-client-h264` passes, including the decode oracle.
- [ ] `cargo test -p ghostframe-client-gpu` passes, including both GPU oracles.
- [ ] Both oracles were mutation-checked and shown to fail when the thing they test is broken.
- [ ] `cargo test -p ghostframe-e2e --test h264` passes against a live server.
- [ ] `just ci-local` is green and `just containers-build` succeeds.
- [ ] §10 of the spec carries measured numbers, not estimates.
- [ ] No tolerance was added to either oracle. If one was, the spec says why, with the measurement behind it.
