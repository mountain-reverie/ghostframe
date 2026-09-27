# `cros-codecs` patches for the V4L2 H.264 backend

Five patches against **`cros-codecs 0.0.6`** exactly as published on crates.io.
`ghostframe-client-h264`'s `v4l2` backend needs all five: without them the crate
either does not build or panics on a live render thread.

All five are upstreamable, and written that way — no ghostframe-specific hooks,
no environment variables, `None` preserving every existing behaviour. Each fixes
a defect that is visible from the outside and describes it in its own commit
message.

## The series

| | Patch | Without it |
| --- | --- | --- |
| 0001 | `v4l2: let the caller name the video device` | The scan picks the first `/dev/videoN` with an OUTPUT mplane queue — the hantro **encoder** on RK3399 — and decode fails with `Unrecoverable decoding error` |
| 0002 | `video_frame: delegate num_planes through PooledVideoFrame` | A `num_planes` override is silently dropped behind the pool; QBUF fails `NumPlanesMismatch(2, 1)` |
| 0003 | `image_processing: gate the MM21 NEON path on the architecture` | `--features v4l2` does not compile on x86-64 at all |
| 0004 | `v4l2: report a stalled decode instead of panicking` | A driver that does not return a buffer takes the session down via `panic!` on the render thread |
| 0005 | `v4l2: implement DecodedHandle::is_ready` | It is `todo!()`, so asking whether a frame is decoded panics |

0001 is the one that shaped ghostframe's code most: it replaced a much longer
mechanism that set an environment variable and then predicted which node the
scan would settle on, refusing a mismatch. See `ghostframe-client-h264`'s
`v4l2_device.rs`, which lost half its size to it.

## Upstream status

Not submitted. Upstream is
[github.com/chromeos/cros-codecs](https://github.com/chromeos/cros-codecs),
whose last commits are Gerrit merges from **March 2025** and whose last push was
**June 2025** — so GitHub looks like a mirror of ChromiumOS Gerrit that stopped
syncing. Contributing requires a Google CLA, which is not something this
repository can sign on anyone's behalf. All five defects are still present at
HEAD, and 0.0.6 is still the newest published version.

The practical consequence: **treat this delta as long-lived.** It is the reason
the patches are a readable series with real commit messages rather than one
squashed diff.

## Is there a better-maintained alternative? No — surveyed 2026-09-27

Upstream being dormant is a maintenance problem, so this was checked properly
rather than assumed. Every candidate, and why none of them is the answer:

| Candidate | Fixes | Verdict |
| --- | --- | --- |
| `nuxodecs 0.1.4` | 0004, 0005; 0003 **partially** | Actively maintained (0.1.2→0.1.4 in Sept 2026) but one maintainer, 1 star, 0 forks. Leaves 0001 and 0002. Its 0003 gates only the `use` statement, so `detile_row`'s `uint8x16_t` / `vld1q_u8` / `vst1q_u8` still fail on x86 one error later — it *looks* fixed to a grep and is not |
| `cros-codecs-extended 0.0.5-extended.2` | **none of the five** | Based on 0.0.5, older than what we use. Last push Aug 2026, 0 stars |
| `cros-codecs-generic-vaapi` | n/a | VA-API surface work; nothing to do with V4L2 |
| ffmpeg `v4l2request` hwaccel | all of them, by not needing the crate | **Dead end in practice.** Would reuse our existing ffmpeg decoder and drop this dependency entirely — but Arch Linux ARM's ffmpeg PKGBUILD does not pass `--enable-v4l2-request` even at 9.0.2, so no distro build has the hwaccel. Requires shipping a custom ffmpeg, i.e. moving the burden onto every user |
| GStreamer `v4l2slh264dec` | all of them, by not needing the crate | **The real alternative, but it needs GStreamer ≥ 1.24.1** — see below. Distro-maintained, zero patches, and it hands over the plane layout so the chroma-offset trap disappears entirely. Cost is a heavy runtime dependency and a different integration model |

So switching forks would trade a dormant, Google-authored, *pinned* crate for an
active one-person fork — while still carrying two or three patches, absorbing
0.1.x API churn, and inheriting changes nobody here has reviewed. That is worse,
not better.

**The mitigation is not a different crate; it is that this delta is a readable
series against a frozen version.** A pinned dependency that never changes is also
a dependency that never breaks under us. What dormancy actually costs is the
option the design doc counted on — "an upstreamed patch reduces the carried delta
to zero" — and that is now off the table, which is recorded in the design's risk
section rather than left as a stale hope.

### The GStreamer exit has a version floor, measured 2026-09-27

The design doc recorded GStreamer as "bit-exact vs software, 60 frames,
dmabuf-backed". **That was measured at 640x480 only, and it does not generalise.**
Probed properly with `tools/hw-probe/gst-dmabuf-rs`:

| | GStreamer 1.22.10 (what the reference machine had) |
| --- | --- |
| 640x480 | dmabuf, bit-exact through the fd, 60 frames over 11 pooled fds, layout from `GstVideoMeta`: `offsets [0, 307200] strides [640, 640] size 614400` |
| 1920x1080 | **`SystemMemory`, 3110400 bytes — a full-frame CPU copy per frame**, silently, at the resolution sessions actually run at |

Three levers were tried and none helps on 1.22:

- Requiring `video/x-raw(memory:DMABuf)` — `v4l2slh264dec` does not advertise the
  feature, so naming it fails to **link**, not to negotiate. `dmabuf_probe.py`'s
  header already said this; it was rediscovered the slow way.
- Enlarging the appsink pool past the 11 buffers it cycles.
- Pinning `format=NV12` to rule out a tiled-format conversion (the decoder also
  offers `NV12_4L4`, `NV12_32L32`, `NV12_16L32S`).

GStreamer **1.24.1** fixed this — *"v4l2codecs: decoders: Add DMA_DRM caps
support"* — which is the mechanism for *requiring* dmabuf rather than hoping for
it. So the exit is real, but it carries a runtime floor of 1.24.1 and cannot be
taken on an older stack.

For contrast, and this is the comparison that matters: the cros-codecs path in
this repo already delivers zero-copy **linear NV12 at 1080p**, measured bit-exact
against a software golden. Switching to GStreamer on a 1.22 stack would be a
regression, not an upgrade.

## Applying them

```sh
# 1. A pristine 0.0.6 to patch.
cargo fetch    # anywhere, to populate ~/.cargo/registry
cp -r ~/.cargo/registry/src/*/cros-codecs-0.0.6 ~/work/cros-codecs-work
chmod -R u+w ~/work/cros-codecs-work
rm -rf ~/work/cros-codecs-work/{target,.cargo}

# 2. The series, in order.
cd ~/work/cros-codecs-work
git init -q && git add -A && git commit -qm "0.0.6 as published"
git am /path/to/ghostframe/third_party/cros-codecs-patches/*.patch

# 3. Point the workspace at it. Untracked -- see .gitignore.
cat > /path/to/ghostframe/.cargo/config.toml <<'EOF'
[patch.crates-io]
cros-codecs = { path = "/home/you/work/cros-codecs-work" }
EOF
```

`git am` rather than `patch -p1` so the commit messages survive — they are the
part worth keeping if these ever go upstream.

**Why a machine-local `[patch.crates-io]` and not something committed.** A
committed patch entry has to name a path or git revision that exists on every
machine and in CI. Vendoring the crate instead means 5.5 MB and ~49k lines of
third-party code in-tree, compiled by every CI job. Neither is worth it for a
feature that only one machine can run, so the workspace builds straight from
crates.io by default and CI compile-checks accordingly.

## What CI can and cannot check

`cargo clippy -p ghostframe-client-h264 --no-default-features --features v4l2`
runs **only on aarch64** (`just check-v4l2`). On x86 the crate carries a
`compile_error!` explaining why, rather than letting the failure surface as
`could not find aarch64 in arch` from a dependency.

Patch 0003 lifts that restriction for anyone who applies the series — the
patched crate `cargo check`s clean for `x86_64-unknown-linux-gnu`, verified, and
mutation-checked by reverting just `image_processing.rs` and watching it fail.
CI cannot benefit while it builds from crates.io. If these ever land upstream,
the `compile_error!` and this paragraph both come out.

## Verifying a patched checkout

```sh
cd ~/work/cros-codecs-work
cargo build --no-default-features --features v4l2                        # aarch64
cargo check --target x86_64-unknown-linux-gnu --no-default-features --features v4l2
```

Then, from the ghostframe workspace, the 12 hardware tests:

```sh
just test-client-v4l2
```

**Mutation check for 0001, recorded:** made `enumerate_devices()` `panic!` and
re-ran those tests. All passed — the scan is never reached, which is the only
real proof that naming the device works rather than merely coinciding with what
the scan would have chosen. On the reference machine the scan happens to pick
correctly on some boots, so a passing test proves nothing on its own.
