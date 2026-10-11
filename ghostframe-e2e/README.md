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

## Loss injection

Two independent mechanisms, at different layers. Use the right one for the
path you are testing.

### i.i.d. loss (ghostframe-lib, plaintext datagram seam)

Acts inside `IoBridge::send_to_all_sessions` (outbound) and on received
datagrams (inbound), i.e. on ghostframe datagrams *before* QUIC encryption, so
it can select by datagram kind. Present only in the Docker e2e suite (a real
server reading its environment). Browserless scenes build the `IoBridge` with a
default config, so these variables do NOT act there; use the burst variables
below or `NetProfile::loss`.
`<DIR>` is `OUTBOUND` or `INBOUND`:

- `GHOSTFRAME_<DIR>_LOSS_PROBABILITY` - f32 in `[0.0, 1.0]`, default `0.0`
- `GHOSTFRAME_<DIR>_LOSS_PREDICATE` - `all` / `tile` / `ack` /
  `palrle_bundled` / `palrle_thin`, default `all`
- `GHOSTFRAME_<DIR>_LOSS_SEED` - u64, default `0`

(Source of truth: `loss_injector_from_lookup` in `ghostframe-lib/src/config.rs`.)

### Gilbert-Elliott burst loss (harness netsim, QUIC packet level)

Acts in the browserless harness's `NetSim`, on **QUIC packets** after
encryption, so no predicate is possible. Browserless only: the Docker path
cannot use these variables (the route there would be `tc netem ... loss
gemodel` in `tests/containers/test-server/entrypoint.sh`). The names differ
from the i.i.d. ones on purpose so one variable can never trigger both.

- `GHOSTFRAME_<DIR>_BURST_LOSS_PROBABILITY` - long-run average loss in
  `(0, 1)`. Unset means no burst model for that direction.
- `GHOSTFRAME_<DIR>_BURST_MEAN_LEN` - mean loss-run length in datagrams,
  `>= 1`; required when the probability is set.
- `GHOSTFRAME_<DIR>_BURST_LOSS_BAD` - loss probability in the bad state,
  default `1.0` (with 1.0, bad-state sojourn equals the loss run).
- `GHOSTFRAME_<DIR>_BURST_SEED` - u64 salt XORed into the direction's
  `NetSim` seed, default `0`.

`OUTBOUND` is server to client, `INBOUND` client to server. Malformed or
infeasible values panic rather than silently running a clean link. Scenes
that must not depend on process-global env use
`run_browserless_with_links` with explicit `LinkOverrides`. See
`src/netsim/burst.rs` for the derivation of the transition probabilities;
note a mean length of 1 is anti-clustered (never two adjacent drops), not
i.i.d.
