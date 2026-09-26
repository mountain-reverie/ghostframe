# Ghostframe attach mode — design

**Status:** approved for planning
**Date:** 2026-09-25
**Related:** [M4a](2026-09-24-native-client-m4a-design.md) (single-client sessions), [M4b](2026-09-24-native-client-m4b-design.md) (display negotiation)

A second deployment mode: the daemon captures an **existing** X session instead of
standing up a headless one of its own.

---

## 1. Why

The headless deployment runs a second X server, and on a machine whose GPU also
drives a local session that is structurally unstable. A DRM device has exactly
one master at a time, and VT switching hands it between X servers — so two X
servers cannot both drive one GPU. Observed on evangeline over one afternoon:

- the Ghostframe X server activated its VT on every start, taking the console
  away from the local session (no `-novtswitch`);
- it lit up whatever physical connector reported connected, so a PiKVM saw the
  Ghostframe desktop instead of the console (no `ZaphodHeads`);
- with mastership contended, **both** Enlightenment instances crashed rather than
  one going dark;
- and the whole session vanished when the guest autologin ended, because nothing
  enabled linger.

Each of those has a fix, and three of them are now merged. But they are fixes for
a problem the deployment creates. On this machine the problem is avoidable:
**Ghostframe is the primary workload.** The physical output is a recovery and
comparison tool — a PiKVM, laggy and low quality, used when something has gone
wrong. The isolation the headless design pays for is protecting something that
does not need protecting.

Attach mode also makes the recovery tool *useful*: the PiKVM shows exactly what
the remote client sees, rather than a different desktop entirely.

### 1.1 What this is NOT

**Not a replacement.** `headless` stays the default. A genuinely headless host —
no local session, no monitor — is what that mode is for, and it remains the right
answer there.

**Not isolation.** See §6. Attach mode deliberately serves the operator's own
desktop.

---

## 2. Architecture

Attach mode installs **one** unit where headless installs three:

```
headless:  ghostframe-xorg ──► ghostframe-wm ──► ghostframe-xdaemon
           (user: guest, DISPLAY=:1, its own X server and WM)

attach:                                         ghostframe-xdaemon
           (user: the username given to install.sh, DISPLAY=:0,
            no X server of its own)
```

Selected by `install.sh --mode attach|headless`, default `headless`.

"The session user" throughout means **the username passed to `install.sh`** — the
same argument the headless mode uses for the guest account. In attach mode it must
be the account that owns the graphical session, because the daemon runs as a user
unit inside it. `install.sh` should verify that account has a graphical session (or
that lightdm autologin is being configured for it) rather than accepting any user
and failing later at connect time.

**Dropped in attach mode**, not merely unused:

| dropped | why it existed |
|---|---|
| `ghostframe-xorg.service` | ran the second X server |
| `ghostframe-wm.service` | ran Enlightenment in that server |
| the `guest` user | owned the headless session |
| `getty@tty1` autologin drop-in | kept the guest session alive |
| `Xwrapper.config` loosening | let a user unit launch Xorg |
| `amdgpu virtual_display=` / VKMS | provided a virtual connector |

The `Xwrapper.config` entry is worth calling out: `allowed_users=anybody` plus
`needs_root_rights=yes` is a global loosening of who may start an X server with
root rights, and it exists *solely* so a systemd user unit could launch Xorg.
Attach mode needs neither.

To be precise, because §5 qualifies this: a **fresh** attach install never writes
that file. **Switching** an existing headless install to attach leaves the file in
place, because the script cannot know nothing else depends on it — it prints that
the setting is now unnecessary instead.

**Changed:** `Environment=DISPLAY=:1` becomes `DISPLAY=:0`.

**Kept:** `GHOSTFRAME_X11_CAPTURE_ONLY=1`, but **for a different reason than in
headless mode**, and this section originally got that backwards.

The headless rationale is that a DRM read would see the host's real scanout
*instead of* the intended display — a content mismatch. **That does not apply
here.** In attach mode the captured session *is* the physical display, so a DRM
read would see the right pixels. Saying the original rationale is "more pointed
here" was self-contradictory: it depended on the scanout not being the target.

The variable stays set for a narrower, privilege-based reason. The zero-copy
writeback path needs an atomic commit, which needs DRM master, and X holds it —
`drm_capture.rs` says so explicitly and probes for it with a `TEST_ONLY` commit
that returns `EACCES` when Xorg is master. That leaves the modesetting-FB
fallback, which is also expected to fail for an unprivileged user unit, since
`GETFB2`'s GEM-handle disclosure is gated on DRM-master-or-`CAP_SYS_ADMIN` and
the daemon is neither. That second failure is reasoned, not measured.

So: the X path is the known-working one, not a proven-optimal one. Whether any
DRM path is viable in attach mode is unexplored, and anyone who measures it
should replace this paragraph with the measurement.

Verified on evangeline: `xprop -root` on `:0` responds with the session owner's
`XAUTHORITY`, and no compositor owns `_NET_WM_CM_S0`, so capture takes the same
plain `GetImage(root)` strategy already working in the headless session. If a
compositor is enabled later, `pick_strategy` handles that case; nothing here
depends on its absence.

---

## 3. Resolution clamping

M4b negotiation drives `RRSetScreenSize` on the session it captures. In attach
mode that is the **physical** display — the one the PiKVM captures. An
unconstrained remote client could therefore set a mode the recovery view cannot
capture, removing the recovery path at precisely the moment it is wanted.

**`GHOSTFRAME_ATTACH_MAX_RESOLUTION`, default `1920x1080`, set to `none` to
disable.**

It lowers what `DisplayController::ceiling()` reports, so M4b's existing
clamp → align-down → floor pipeline and all of its tests apply unchanged. No new
clamping logic, and no second place where a requested mode can be altered.

Default on, disableable: the clamp protects the recovery view until the operator
has enough confidence in the setup to trade that for full remote resolution.

**Scope:** the variable is honoured wherever it is set, but only *installed* by
attach mode. Headless mode has no physical output to protect, so it leaves the
variable unset and M4b's ceiling behaves exactly as it does today. This avoids a
second code path — one ceiling mechanism, one optional lowering of it.

---

## 4. Session lifecycle

**lightdm autologin** for the session user, so `:0` exists after an unattended
boot. The daemon is a **user unit in that session**, so it starts and stops with
it.

| event | behaviour |
|---|---|
| boot | lightdm autologins, `:0` comes up, daemon starts |
| screen locked | the lock screen is served as-is |
| logout | the daemon stops with the session |
| re-login | the daemon starts again |

Serving the lock screen is correct, not a gap: a remote view of a real desktop
should show that the desktop is locked.

No linger required — this is a real login session, which is what made the guest
path fragile. The guest session ended and took every user unit with it, with no
obvious cause.

**Cost, accepted:** an unattended boot leaves the machine logged in. On a host
whose purpose is to serve that session remotely, that is the point rather than a
regression.

---

## 5. Mode switching must clean up

`install.sh` **must disable and remove the other mode's units.** Leaving them
installed means both try to run, which reproduces exactly the two-X-server
contention attach mode exists to avoid — and it would look like a new bug rather
than a leftover.

- switching to `attach`: stop, disable and remove `ghostframe-xorg.service` and
  `ghostframe-wm.service`
- switching to `headless`: remove the attach unit

It will **not** auto-revert `Xwrapper.config`. That is a global file the script
cannot know is unused by something else, so it prints that the setting is no
longer needed and leaves the decision to the operator.

---

## 6. Security: this is an escalation, and it should be stated

In headless mode a client that reaches the endpoint gets a sandboxed `guest`
desktop. **In attach mode it gets the operator's own desktop — their files, their
browser sessions, their sudo.**

M4a's single-client eviction still applies, so two clients cannot both attach, but
the tailnet is the only boundary. There is no second gate.

That belongs in three places, not buried in one: this spec, the `install.sh`
output when `--mode attach` is selected, and the mode's unit file. An operator who
picks attach mode should not be able to do so without reading it.

---

## 7. Testing

- **The clamp is pure logic**: unit tests for `GHOSTFRAME_ATTACH_MAX_RESOLUTION`
  parsing (`1920x1080`, `none`, malformed input) and for the resulting ceiling.
- **Mode-switch cleanup is testable without hardware**: run `install.sh` both ways
  in sequence against a throwaway prefix and assert which unit files exist after
  each. This is the part most likely to rot, because it only matters on a
  transition.
- **Everything else needs the real machine**: capture from `:0`, autologin,
  lock-screen behaviour, and whether the PiKVM holds its view across a restart.
  Say so rather than implying coverage.

---

## 8. Risks

1. **Both modes installed at once** (§5). The failure looks like the original
   contention bug, so the cleanup is load-bearing and gets a test.
2. **Autologin leaves an unattended machine logged in** (§4). Deliberate, stated.
3. **Remote input drives the real desktop** (§6). Deliberate, stated three times.
4. **`:0` is not guaranteed to be the session's display.** A second seat or a
   changed lightdm config could move it. The unit hardcodes `:0`; if that proves
   insufficient, detection belongs in `install.sh` where the other environment
   probing already lives.
5. **The clamp default may surprise.** A user with a 1440p client will get 1080p
   and may not know why. The install output should name the variable.
