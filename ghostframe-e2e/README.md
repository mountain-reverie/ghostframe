# ghostframe-e2e

End-to-end tests for the ghostframe daemon + web client. Containers + tsnet +
Weston + browser drive a full scenario; assertions read pixels and server
telemetry.

## Firefox path

The e2e suite runs every static-codec scenario on both Chromium (via CDP) and
Firefox (via WebDriver). Firefox tests have the `_firefox` suffix; Chromium
tests have `_chromium`.

### Prerequisites

- `firefox` (or `firefox-esr`) on PATH, or `GHOSTFRAME_E2E_FIREFOX_BIN` set
- `geckodriver` v0.32+ on PATH
- `certutil` from NSS tools (Arch `nss` / Debian `libnss3-tools` / Fedora `nss-tools`)

Run `just e2e-firefox-doctor` to check all three.

### Running

```bash
just e2e-firefox            # Firefox only
cargo test -p ghostframe-e2e --test e2e -- --skip _firefox   # Chromium only
cargo test -p ghostframe-e2e --test e2e                       # both
```

### H.264 tests are Chromium-only

Firefox WebGPU lacks the `TEXTURE_EXTERNAL` capability that the H.264 blit
shader requires. Tests that depend on H.264 (`e2e_h264_*`,
`e2e_headroom_guard_forces_h264`, `e2e_loss_override_forces_h264`,
`e2e_mode_switch`) have a `_chromium` entry only. Once the `h264Supported`
HELLO bit is plumbed (separate work), the server will stop selecting H.264
for Firefox clients and these tests can grow Firefox siblings.

### Spec + plan

- Design: `docs/superpowers/specs/2026-06-12-firefox-e2e-design.md`
- Plan: `docs/superpowers/plans/2026-06-12-firefox-e2e.md`
