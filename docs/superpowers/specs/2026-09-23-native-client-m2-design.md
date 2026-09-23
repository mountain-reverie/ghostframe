# Native client M2: CLI, tailnet lifecycle, and the showcase window

*Design, 2026-09-23. Builds on
`docs/superpowers/specs/2026-09-22-native-client-design.md` (M1, merged as
PR #94), which this supersedes only where explicitly noted.*

## 1. What M2 delivers

M1 produced a library: it decodes on the GPU, publishes an exported dmabuf, and
proves it with pixel assertions against a live server. It is not yet something a
person can use.

M2 makes it usable — a tailnet you can log into, a window you can watch, and
input that reaches the remote:

- **A.** `ghostbridge` gains interactive login and real logout; `ghostframe-tsnet`
  gains the safe wrappers.
- **B.** The `ghostframe` CLI: `login`, `logout`, `connect`.
- **C.** The showcase window: fullscreen, 1:1, chorded quit and minimize.
- **D.** The fence-export upgrade deferred from M1 §5.5 — **measured before
  built**.

C and D compose deliberately. The showcase is the first consumer that actually
presents frames, so it is the first place `ExportRing::publish`'s blocking
`poll(Wait)` becomes observable. D is therefore gated on a measurement rather
than scheduled outright; see §5.

### Non-goals

- Scaling the remote image (see §3.1).
- Inhibiting compositor shortcuts (see §3.3).
- Clipboard, audio, multi-monitor, resolution negotiation.

## 2. ghostbridge and the CLI

### 2.1 Two new Go exports

```
gbridge_login_url(sd, buf, len)   // surface the Tailscale auth URL
gbridge_logout(sd)                // LocalClient().Logout
```

`gbridge_new` already accepts an authkey, which is the only auth path today.
That covers CI but not a person.

**`logout` must be a real logout.** Deleting the state directory leaves the node
registered and visible in the tailnet's device list with no way to reach it;
`Logout` removes it. Order matters at the call site too: log out first, *then*
delete local state, or the local credentials needed to log out are gone.

The exact `tsnet` API differs across releases — interactive login is
`StartLoginInteractive` plus status polling in some, a blocking `Up(ctx)` with a
readable `AuthURL` in others. Implementation checks the version pinned in
`ghostbridge/go.mod` rather than assuming, and covers both exports with Go tests
in the style of `web_server_test.go`.

### 2.2 The CLI

`clap` derive, matching `ghostframe-test-pattern`. State lives under
`$XDG_STATE_HOME/ghostframe/tsnet` (default `~/.local/state/...`).

```
ghostframe login  [--authkey K] [--login-server URL] [--hostname H]
ghostframe logout
ghostframe connect <host> [--port N] [--chord-prefix PREFIX]
```

- `login` without `--authkey` prints the auth URL and blocks until the node
  reports Running. With one, it is non-interactive for CI.
- `connect` requires seeded state and **exits non-zero with a plain message** if
  absent, rather than hanging on a dial that can never succeed.

**Errors are messages, not panics.** `main` returns `Result` and the top level
maps `ClientError` to a one-line diagnosis and an exit code. A CLI that greets
"you are not logged in" with a backtrace is user-hostile, and this one is the
first thing a person touches.

`connect` builds its own tsnet node. `Client::attach_bridge` exists for embedders
already on the tailnet — and for the e2e harness, which must share its node — but
a user running `ghostframe connect` is not one of those.

## 3. The showcase window

### 3.1 Fullscreen, 1:1, centred

**Supersedes M1 §10's "opens at the remote resolution".** The window now starts
fullscreen: `xdg_toplevel.set_fullscreen` before the first commit on Wayland,
`_NET_WM_STATE_FULLSCREEN` on X11.

The remote image is drawn at **native size, centred, with the surplus cleared
black** — no scaling. On a display matching the remote resolution (1920x1080 on
1920x1080, the common case) this is pixel-perfect edge to edge and
indistinguishable from scaling.

Scaling remains an explicit non-goal, for the reason M1 gave: it introduces a
coordinate-math bug class into code whose job is to prove the API. With
letterboxing the mapping is a pure offset:

```
remote_x = window_x - origin_x
remote_y = window_y - origin_y
```

**Pointer events outside the image rectangle are clamped to its edge**, not sent
as negative coordinates. The wire carries `i16`, so a negative value is a real
coordinate the server would act on rather than an obvious error.

**Who draws the black differs by backend, and on Wayland it is free.**

On Wayland we attach the library's dmabuf directly as the `wl_buffer`, so the
surface is the remote resolution. xdg-shell requires that a fullscreened surface
smaller than the output be **centred by the compositor with the remainder filled
black** — which is exactly the presentation we want, at no cost. We do not
composite a background, use a subsurface, or touch `wp_viewporter`.

We still compute the centring offset ourselves, from the output size reported by
`wl_output`/`xdg_toplevel.configure` against the buffer size, because input
coordinates must be mapped back and the compositor does not tell us where it put
the surface.

On X11 there is no such guarantee: the fullscreen window is output-sized and we
present a smaller `Pixmap` into it, so the surround is ours to clear. Clear it on
configure and on any resolution change — **not per frame**, and note it must also
be cleared the first time each `buffer_id` is presented if the surround were ever
part of the presented pixmap (it is not, since we present the dmabuf pixmap into
a region of an already-cleared window).

Only the damage rects the library reports are re-presented.

### 3.2 Chords

Default prefix `Ctrl+Alt+b`; `--chord-prefix super+b` selects the Super variant.

| Sequence | Action |
|---|---|
| prefix, then `d` | quit |
| prefix, then `h` | minimize |

A two-state machine. **Every key, including the prefix, is forwarded to the
remote as normal; only the completing `d`/`h` is swallowed.** This keeps input
latency and ordering untouched, at the cost of the remote occasionally seeing a
stray `b` — the same bargain tmux makes with its prefix.

Any other key after the prefix disarms the machine and is forwarded normally.

`h` is not decoration. Fullscreen with every key forwarded is precisely the state
in which a user gets stuck, and minimize is a gentler escape than quitting a live
session. Window close and `SIGINT` remain unconditional escapes regardless of
chord state.

### 3.3 Why we do NOT inhibit compositor shortcuts

`zwp_keyboard_shortcuts_inhibit_v1` exists (in `wayland-protocols`) and is
designed for exactly this: it would let `Super+b` reach us *and* forward every
other Super combo to the remote session, which is what a remote-desktop user
ultimately wants.

It is deliberately out of scope. It is a request the compositor may refuse, it
needs an equivalent path on X11 (`grab_keyboard`), and it buys reliability for a
chord we can simply choose not to collide with. `Ctrl+Alt+b` reaches the
application on every compositor without protocol work.

Recorded here because it is the right answer for a *product* client that wants
the remote's own Super shortcuts to work, and the next person to want that
should find the pointer rather than rediscover the protocol.

### 3.4 Backends

Runtime switch: `WAYLAND_DISPLAY`, then `DISPLAY`, else a clear error.

Wayland is `smithay-client-toolkit` 0.21 — `dmabuf`, `seat::keyboard` (behind
its `xkbcommon` feature), `shell`, registry. Keysyms need no translation table:
SCTK yields `xkeysym::Keysym`, whose `raw()` is the X11 keysym the wire carries.

X11 is `x11rb` + `xkbcommon`'s `x11` feature, with `dri3::pixmap_from_buffers`
and `present::pixmap`, one `Pixmap` cached per `buffer_id`. No DRI3/Present
helper crate exists; ~150 lines is the floor. The two backends are not symmetric
in maintenance cost and that is accepted rather than equalised.

`DmabufFeedback` supplies the compositor's preferred modifiers, which feed
`gf_client_config.preferred_modifiers`.

## 4. Testing

| Layer | How |
|---|---|
| ghostbridge exports | Go tests, in `web_server_test.go`'s style |
| CLI arg parsing | unit tests, no tailnet |
| **Chord state machine** | **unit tests** |
| Window + first frame | automated smoke under `spawn_weston_headless` |
| X11 backend | documented manual verification |
| Frame pacing | instrumented measurement, feeding §5 |

The chord machine is called out because it is pure logic and because its failure
mode — "a key I pressed vanished" — is miserable to diagnose interactively and
trivial to pin with a table of `(input sequence) -> (forwarded, action)`.

This development machine has **no desktop session** (`loginctl` reports
`Type=tty`), so the Wayland smoke test runs under the harness's existing
`spawn_weston_headless`: launch the CLI against a live server, assert the surface
maps and a frame is presented, drive the quit chord, assert clean exit. A second
headless stack for the X11 path is not worth it for ~150 lines.

## 5. Fence export: measure, then decide

`ExportRing::publish` blocks on `device.poll(PollType::wait_indefinitely())` and
reports `acquire_fence_fd = -1`. M1 shipped that deliberately; the field exists
so the upgrade is not an ABI break.

**Measure first.** Once the showcase presents frames, instrument published-frame
intervals and report p50/p99 over a 60-second 1080p session.

- **Clean at 60 Hz** — the upgrade stays deferred and M2 ends smaller. This is a
  live possibility: the blit is ~8 MB and the stall may sit well inside a frame
  budget.
- **Visible stalls** — implement it:
  `wgpu_hal::vulkan::Queue::add_signal_semaphore` on the blit submission,
  exported with `vkGetSemaphoreFdKHR`, requiring `VK_KHR_external_semaphore_fd`.
  The showcase then becomes the first consumer that actually waits on the fence,
  which is also the only way to know the export side is correct.

Optimising a number nobody has measured is how a day disappears for no gain.

## 6. Risks

1. **`Super+b` is unreachable on the user's compositor** even as an opt-in.
   Mitigated by the default prefix; the flag is a convenience, not a dependency.
2. **SCTK's dmabuf path may not accept our modifier.** M1 exports
   `DRM_FORMAT_MOD_LINEAR` on this hardware (RADV Polaris lacks
   `VK_EXT_image_drm_format_modifier`), which every compositor imports, but a
   tiled export on other hardware could be refused. `DmabufFeedback` is the
   negotiation path and M1 already accepts a preference list.
3. **X11 DRI3 is hand-written and manually verified**, so it will rot faster than
   the Wayland path. Dropping it is cheaper than replacing it if it becomes a
   burden.

4. **X11 almost certainly shows red and blue swapped.** Found while implementing
   Task 8, confirmed by analysis, not yet observed on a display.

   The GPU exports `VK_FORMAT_R8G8B8A8_UNORM`, which puts red at the lowest
   address — DRM fourcc `ABGR8888`. Wayland is *told* that explicitly through
   the linux-dmabuf protocol's format field. **DRI3's `PixmapFromBuffers` has no
   format field at all**: the X server infers the layout from `depth`/`bpp` using
   the same fixed table Mesa's DRI3 loader uses, which yields `XRGB8888` —
   blue at the lowest address. There is no channel through which to correct it.

   The fix is a BGRA-ordered export for the X11 path. `copy_texture_to_texture`
   requires matching formats, and `Rgba8Unorm`/`Bgra8Unorm` are not a legal
   view-format pair, so it needs a swizzling blit (a render pass reusing the
   `present_blit` machinery) rather than a format tweak — roughly 40 lines in
   `ghostframe-client-gpu`, plus a per-backend export-format choice.

   **Verify on the first manual X11 run before building the fix.** If the colours
   are correct, this analysis is wrong somewhere and the fix would introduce the
   very bug it is meant to prevent. This is the same red/blue transposition class
   that produced long-misdiagnosed e2e flakes before `847d870` found the real
   cause, so it is worth confirming rather than assuming in either direction.
4. **The tsnet login API differs across releases** — checked against the pinned
   version rather than assumed.
