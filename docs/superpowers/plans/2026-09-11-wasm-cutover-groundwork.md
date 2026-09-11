# Wasm Cutover Groundwork Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the two reversible prerequisites for the wasm cutover — one place to build the web client, and a `client-core` mode that emits validated-but-undecoded tile payloads.

**Architecture:** Steps 1 and 2 of `docs/superpowers/specs/2026-09-11-wasm-cutover-design.md`. Neither touches the browser. `TileDelivery::Decoded` stays the default, so every current consumer is unaffected; `Payload` is added and tested but has no caller until the wasm crate exists.

**Tech Stack:** GitHub Actions composite actions, Rust (`ghostframe-client-core`).

---

## Before you start

Run cargo with `export TMPDIR=/home/cedric/.cache/ghostframe-tmp` (mkdir -p it first) in the same shell invocation. `/tmp` fills on this machine and produces a confusing build.rs panic from ghostbridge's Go link step.

Run cargo in the **foreground** with an explicit timeout. Never `git add -A`.

Do **not** run `just containers-build` for anything in this plan.

**Where tests go.** `ghostframe-client-core` keeps its tests as *integration*
tests in `ghostframe-client-core/tests/`, not as `mod tests` blocks in `src/`
(the sole exception is `src/input.rs`). Every test in this plan goes in one new
file, `ghostframe-client-core/tests/tile_delivery.rs`.

There is no shared `tests/common/` module — each file carries its own helpers.
Copy `test_core()` and `tile_datagrams(frame_seq, tx, ty, codec, pass, payload,
max_frag)` from `ghostframe-client-core/tests/timers.rs`, which is the
established pattern; do not hand-roll datagram bytes, because a wrong header
would make these tests pass for the wrong reason.

`cargo test -p ghostframe-client-core` runs every target in the package,
integration tests included, so — unlike `ghostframe-lib`, where CI names
`--test` targets explicitly — this new file needs **no** CI wiring.

## File structure

| File | Responsibility |
|---|---|
| `.github/workflows/_build-web-client/action.yml` | new; the single place the web client SPA is built |
| `.github/workflows/ci.yml`, `e2e.yml`, `nightly.yml` | eleven inline build blocks replaced by uses of the action |
| `ghostframe-client-core/src/lib.rs` | `TileDelivery` enum, `ClientConfig.tile_delivery` |
| `ghostframe-client-core/src/event.rs` | `Event::TilePayload`, `Event::PaletteUpdated` |
| `ghostframe-client-core/src/reassembly.rs` | per-codec payload-mode branches |
| `ghostframe-client-core/tests/tile_delivery.rs` | new; every test in this plan |

---

### Task 1: The composite action, proven on one site

**Files:**
- Create: `.github/workflows/_build-web-client/action.yml`
- Modify: `.github/workflows/ci.yml` (first build site only)

- [ ] **Step 1: Read what you are replacing**

The eleven `npm run build` sites are **not** identical. Verified 2026-09-11:

| site | shape |
|---|---|
| `ci.yml` 44, 88, 116, 165, 198 | plain |
| `e2e.yml` 35, 60, 109 | plain |
| `ci.yml` 137 | plain **plus** `npx tsc --noEmit`, and uploads the `web-client-dist` artifact |
| `e2e.yml` 176 | conditional fallback inside a download-artifact block |
| `nightly.yml` 46 | no adjacent `setup-node`, no npm cache |

**This task consolidates the eight plain sites only.** The three variants are
left exactly as they are and are addressed separately — collapsing them would
silently drop a type-check, an artifact upload, or a conditional.

Each of the eight plain sites is this pair of steps:

```yaml
      - uses: actions/setup-node@v4
        with:
          node-version: '20'
          cache: npm
          cache-dependency-path: ghostframe-web-client/package-lock.json
      - name: build web client SPA (ghostbridge //go:embeds it)
        working-directory: ghostframe-web-client
        run: |
          npm ci
          npm run build
```

Confirm with `grep -n -B9 "npm run build" .github/workflows/ci.yml`. If one of the eight listed as plain turns out to differ, **stop and report it** rather than adapting the action to cover it.

- [ ] **Step 2: Create the action**

```yaml
name: Build the ghostframe web client
description: >
  Builds the web client SPA into ghostframe-web-client/dist/. ghostbridge
  //go:embeds that directory at compile time, so every job that builds or
  runs a ghostframe binary needs it present.

  This exists because the same node-setup-and-build pair was previously
  inlined at eight separate sites across ci.yml and e2e.yml. Three further
  sites differ (an extra type-check plus artifact upload in ci.yml, a
  conditional artifact fallback in e2e.yml, and a cacheless build in
  nightly.yml) and deliberately do NOT use this action.
  The wasm cutover adds a Rust toolchain and wasm-pack to this build; doing
  that eleven times would be eleven chances to miss one, and a missed site
  ships a dist/ whose wasm is absent or stale — which fails at runtime, not
  at build time.

runs:
  using: composite
  steps:
    - uses: actions/setup-node@v4
      with:
        node-version: '20'
        cache: npm
        cache-dependency-path: ghostframe-web-client/package-lock.json
    - name: build web client SPA (ghostbridge //go:embeds it)
      shell: bash
      working-directory: ghostframe-web-client
      run: |
        npm ci
        npm run build
```

Note `shell: bash` — composite actions require it on every `run` step, unlike workflow jobs.

- [ ] **Step 3: Migrate exactly one site**

In `.github/workflows/ci.yml`, replace the **first** occurrence of that step pair with:

```yaml
      - uses: ./.github/workflows/_build-web-client
```

Leave the other ten alone for now.

- [ ] **Step 4: Verify the YAML parses**

Run:

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); yaml.safe_load(open('.github/workflows/_build-web-client/action.yml')); print('OK')"
```

Expected: `OK`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/_build-web-client/action.yml .github/workflows/ci.yml
git commit -m "ci: add a composite action for the web-client build"
```

**Push this and let CI run before Task 2.** One migrated site proves the action works; migrating all eleven and discovering `shell: bash` was missing means eleven broken jobs instead of one.

---

### Task 2: Migrate the remaining seven plain sites

**Files:**
- Modify: `.github/workflows/ci.yml` (remaining sites), `.github/workflows/e2e.yml`, `.github/workflows/nightly.yml`

- [ ] **Step 1: Find every remaining plain site**

```bash
grep -rn "npm run build" .github/workflows/
```

Expected: ten remaining, of which **seven are plain** and must be migrated —
four in `ci.yml` (lines near 88, 116, 165, 198) and three in `e2e.yml` (near
35, 60, 109).

**Do NOT touch these three:**
- `ci.yml` ~137 — also runs `npx tsc --noEmit` and uploads the artifact
- `e2e.yml` ~176 — a conditional fallback inside a download-artifact block
- `nightly.yml` ~46 — no adjacent `setup-node`, no npm cache

- [ ] **Step 2: Replace each**

For each, delete the `actions/setup-node@v4` step *and* the `build web client SPA` step, and put in their place:

```yaml
      - uses: ./.github/workflows/_build-web-client
```

Take care in `e2e.yml`: only remove a `setup-node` step that is immediately followed by the web-client build step. The `setup-node` at ~153 belongs to the artifact-fallback block and must stay.

- [ ] **Step 3: Verify**

```bash
grep -rn "npm run build" .github/workflows/
```

Expected: exactly **three** remaining — the three documented variants, untouched.

```bash
python3 -c "
import yaml
for f in ['ci','e2e','nightly']:
    yaml.safe_load(open(f'.github/workflows/{f}.yml'))
print('OK')"
```

Expected: `OK`.

```bash
grep -c "_build-web-client" .github/workflows/ci.yml .github/workflows/e2e.yml .github/workflows/nightly.yml
```

Expected: 5, 3, 0 — eight total, matching the eight plain sites.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml .github/workflows/e2e.yml .github/workflows/nightly.yml
git commit -m "ci: route every web-client build through the composite action"
```

---

### Task 3: `TileDelivery`, defaulting to today's behaviour

**Files:**
- Modify: `ghostframe-client-core/src/lib.rs`
- Create: `ghostframe-client-core/tests/tile_delivery.rs`

- [ ] **Step 1: Write the failing test**

Create `ghostframe-client-core/tests/tile_delivery.rs` with the helpers copied from `tests/timers.rs` (see "Where tests go" above), then add:

```rust
    /// The default must be the behaviour every current consumer already
    /// relies on. A default of `Payload` would silently stop `ghostframe-e2e`
    /// and the native client receiving pixels.
    #[test]
    fn tile_delivery_defaults_to_decoded() {
        let cfg = ClientConfig {
            indices_raw_enabled: true,
            supports_h264: false,
            ..Default::default()
        };
        assert_eq!(cfg.tile_delivery, TileDelivery::Decoded);
    }
```

- [ ] **Step 2: Run it, confirm it fails**

Run: `cargo test -p ghostframe-client-core --test tile_delivery tile_delivery_defaults_to_decoded`
Expected: FAIL — `TileDelivery` not found, and `ClientConfig` has no `Default`.

- [ ] **Step 3: Implement**

In `ghostframe-client-core/src/lib.rs`, above `ClientConfig`:

```rust
/// Whether the core decodes tiles to pixels, or stops after validation and
/// hands the payload up.
///
/// The browser decodes on the GPU (`ghostframe-web-client/src/webgpu/`, ten
/// WGSL compute shaders), so it wants validated payloads, not RGBA. Native
/// and headless consumers want pixels. One core, two consumers — not two
/// implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TileDelivery {
    /// Decode to RGBA and emit `Event::TileReady`. The default, because it is
    /// what every existing consumer expects.
    #[default]
    Decoded,
    /// Stop after reassembly, parity recovery, prevalidation and generation
    /// checks; emit `Event::TilePayload` with the undecoded bytes.
    Payload,
}
```

and add to `ClientConfig`:

```rust
    /// See [`TileDelivery`]. Defaults to `Decoded`.
    pub tile_delivery: TileDelivery,
```

Add `Default` to `ClientConfig`'s existing derives. It is currently
`#[derive(Debug, Clone, Copy)]`, so it becomes
`#[derive(Debug, Clone, Copy, Default)]` — **keep `Copy`**. Dropping it would
break every call site that passes a `ClientConfig` by value, and `TileDelivery`
derives `Copy` precisely so the containing struct can stay `Copy`.

There are 9 `ClientConfig { .. }` literals across the workspace
(`grep -rn "ClientConfig {" --include=*.rs . | grep -v ./target`). Each needs
the new field or `..Default::default()`. Change only the literals, never an
assertion.

- [ ] **Step 4: Run it, confirm it passes**

Run: `cargo test -p ghostframe-client-core`
Expected: PASS. Every existing `ClientConfig { .. }` literal in the workspace needs the new field or `..Default::default()`; fix the literals, change no assertions.

- [ ] **Step 5: Verify nothing else moved**

```bash
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1
```

Expected: 390 and 10, both unchanged.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-core/src/lib.rs
git commit -m "feat(client-core): add TileDelivery, defaulting to Decoded"
```

---

### Task 4: The payload events

**Files:**
- Modify: `ghostframe-client-core/src/event.rs`
- Modify: `ghostframe-client-core/tests/tile_delivery.rs`

- [ ] **Step 1: Write the failing test**

Add to `ghostframe-client-core/tests/tile_delivery.rs`:

```rust
    /// `TilePayload` carries everything the GPU decoder needs to know that it
    /// should not have to re-derive: which tile, which pass, which
    /// generation, which codec.
    #[test]
    fn tile_payload_carries_the_gpu_decoders_inputs() {
        let e = Event::TilePayload {
            frame_seq: 7,
            tile_x: 1,
            tile_y: 2,
            pass_idx: 3,
            generation: 4,
            codec: ghostframe_protocol::protocol::Codec::PalRle,
            payload: vec![0xAA, 0xBB],
        };
        match e {
            Event::TilePayload { frame_seq, tile_x, tile_y, pass_idx, generation, codec, payload } => {
                assert_eq!((frame_seq, tile_x, tile_y, pass_idx, generation), (7, 1, 2, 3, 4));
                assert_eq!(codec, ghostframe_protocol::protocol::Codec::PalRle);
                assert_eq!(payload, vec![0xAA, 0xBB]);
            }
            _ => panic!("wrong variant"),
        }
    }

    /// The palette shadow is protocol state and moves into the core, but
    /// `palrle_decode.wgsl` needs the table to decode. Without this event the
    /// core would own the palette and the shader could not see it — which
    /// shows up as wrong colours, not an error.
    #[test]
    fn palette_updated_carries_the_slot_and_colours() {
        let e = Event::PaletteUpdated {
            palette_id: 5,
            colors: vec![[1, 2, 3, 255], [4, 5, 6, 255]],
        };
        match e {
            Event::PaletteUpdated { palette_id, colors } => {
                assert_eq!(palette_id, 5);
                assert_eq!(colors.len(), 2);
                assert_eq!(colors[1], [4, 5, 6, 255]);
            }
            _ => panic!("wrong variant"),
        }
    }
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test -p ghostframe-client-core --test tile_delivery tile_payload_carries`
Expected: FAIL — no variant `TilePayload`.

- [ ] **Step 3: Implement**

Add to `pub enum Event` in `ghostframe-client-core/src/event.rs`:

```rust
    /// A reassembled, parity-recovered, prevalidated, generation-checked tile
    /// pass — but NOT decoded. Emitted instead of `TileReady` when
    /// `TileDelivery::Payload` is configured, so a GPU decoder can take it.
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pass_idx: u8,
        generation: u8,
        codec: ghostframe_protocol::protocol::Codec,
        payload: Vec<u8>,
    },
    /// A palette slot changed. Only emitted under `TileDelivery::Payload`:
    /// under `Decoded` the core applies the palette itself and the consumer
    /// never needs to see it.
    ///
    /// Colours are BGRA, matching the wire and the palette table.
    PaletteUpdated {
        palette_id: u8,
        colors: Vec<[u8; 4]>,
    },
```

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p ghostframe-client-core`
Expected: PASS. Any exhaustive `match` on `Event` elsewhere now fails to compile — add the new arms rather than a catch-all, so a future variant is still a compile error.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-core/src/event.rs
git commit -m "feat(client-core): add TilePayload and PaletteUpdated events"
```

---

### Task 5: Payload mode for Raw and Solid

**Files:**
- Modify: `ghostframe-client-core/src/reassembly.rs:249-288`

- [ ] **Step 1: Write the failing test**

Add to `ghostframe-client-core/tests/tile_delivery.rs`:

```rust
    /// Under Payload mode a Solid tile must arrive as its 4 wire bytes, not
    /// as 4096 bytes of expanded pixels — the GPU's solid.wgsl does the
    /// expansion.
    #[test]
    fn solid_in_payload_mode_emits_wire_bytes() {
        let events = drive_one_tile(
            TileDelivery::Payload,
            ghostframe_protocol::protocol::Codec::Solid,
            &[10, 20, 30, 255],
        );
        let payloads: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::TilePayload { payload, codec, .. } => Some((payload.clone(), *codec)),
                _ => None,
            })
            .collect();
        assert_eq!(payloads.len(), 1, "expected exactly one TilePayload, got {events:?}");
        assert_eq!(payloads[0].0, vec![10, 20, 30, 255]);
        assert!(
            !events.iter().any(|e| matches!(e, Event::TileReady { .. })),
            "Payload mode must not emit TileReady"
        );
    }

    /// The default path must be untouched: the same input under Decoded mode
    /// still produces 4096 bytes of expanded RGBA.
    #[test]
    fn solid_in_decoded_mode_is_unchanged() {
        let events = drive_one_tile(
            TileDelivery::Decoded,
            ghostframe_protocol::protocol::Codec::Solid,
            &[10, 20, 30, 255],
        );
        let ready: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::TileReady { rgba, .. } => Some(rgba.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].len(), 4096);
        // BGRA 10,20,30 -> RGBA 30,20,10,255
        assert_eq!(&ready[0][0..4], &[30, 20, 10, 255]);
    }
```

You need a `drive_one_tile(delivery, codec, payload) -> Vec<Event>` helper. Build it on the `test_core()` / `tile_datagrams(...)` pair copied from `tests/timers.rs`: construct a core with the given delivery mode, build the datagrams for a single-fragment tile, feed them with `core.handle_datagram(&dg, now_us)`, and collect the returned events. `tests/timers.rs:76-80` shows the call shape.

- [ ] **Step 2: Run, confirm the Payload one fails**

Run: `cargo test -p ghostframe-client-core --test tile_delivery solid_in_`
Expected: `solid_in_decoded_mode_is_unchanged` PASSES (it describes today's behaviour), `solid_in_payload_mode_emits_wire_bytes` FAILS.

That split is the point: one test pins what must not change, the other drives what is new.

- [ ] **Step 3: Implement**

In `reassembly.rs`, immediately after the sentinel guard and before `match asm.codec`, add:

```rust
        // Payload mode stops here for the codecs whose decode is purely
        // pixel expansion. Raw and Solid carry no protocol state, so there is
        // nothing to update before handing the bytes up.
        if self.tile_delivery == TileDelivery::Payload
            && matches!(asm.codec, Codec::Raw | Codec::Solid)
        {
            events.push(Event::TilePayload {
                frame_seq,
                tile_x: tx,
                tile_y: ty,
                pass_idx: asm.pass,
                generation: asm.generation,
                codec: asm.codec,
                payload,
            });
            return;
        }
```

`self.tile_delivery` is read from the config — add the field to whichever struct in this file holds the config-derived state, mirroring how `indices_raw_enabled` is already threaded.

- [ ] **Step 4: Run, confirm both pass**

Run: `cargo test -p ghostframe-client-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-core/src/reassembly.rs
git commit -m "feat(client-core): payload delivery for Raw and Solid"
```

---

### Task 6: Payload mode for PalRle, with the palette

The subtle one. Under `Decoded`, `decode_pal_rle_tile` does three things: prevalidate, **apply the palette upsert to the shadow and table**, and expand to pixels. Payload mode must keep the first two — they are protocol state — and drop only the third.

**Files:**
- Modify: `ghostframe-client-core/src/reassembly.rs` (the `Codec::PalRle` arm)

- [ ] **Step 1: Write the failing test**

```rust
    /// Payload mode must still apply the bundled palette upsert — it is
    /// protocol state, not decoding — and must surface it so the GPU can
    /// upload the table. Dropping the upsert would leave the shader decoding
    /// against a stale palette, which shows as wrong colours rather than an
    /// error.
    #[test]
    fn palrle_in_payload_mode_applies_and_reports_the_palette() {
        let mut colors = [[0u8; 4]; 16];
        colors[0] = [10, 20, 30, 255];
        colors[1] = [40, 50, 60, 255];
        let entry = ghostframe_protocol::codec::pal_rle::PaletteEntry { colors, count: 2 };
        let packed = [0u8; 512];
        let bundled = ghostframe_protocol::codec::pal_rle::encode_pal_rle_payload(
            &packed, &entry, 5, true,
        );

        let events = drive_one_tile(
            TileDelivery::Payload,
            ghostframe_protocol::protocol::Codec::PalRle,
            &bundled,
        );

        let updated: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::PaletteUpdated { palette_id, colors } => Some((*palette_id, colors.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(updated.len(), 1, "expected one PaletteUpdated, got {events:?}");
        assert_eq!(updated[0].0, 5);
        assert_eq!(updated[0].1[0], [10, 20, 30, 255]);
        assert_eq!(updated[0].1[1], [40, 50, 60, 255]);

        assert!(
            events.iter().any(|e| matches!(e, Event::TilePayload { .. })),
            "the tile payload itself must still be emitted"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::TileReady { .. })),
            "Payload mode must not emit TileReady"
        );
    }
```

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test -p ghostframe-client-core --test tile_delivery palrle_in_payload_mode`
Expected: FAIL — no `PaletteUpdated` emitted.

- [ ] **Step 3: Implement**

Replace the `Codec::PalRle` arm's body so that under `Payload` it prevalidates, applies the upsert, emits `PaletteUpdated` and `TilePayload`, and skips expansion:

```rust
            Codec::PalRle => {
                if self.tile_delivery == TileDelivery::Payload {
                    match prevalidate_pal_rle(&payload, &self.palette_shadow) {
                        Ok(validated) => {
                            // Apply the upsert exactly as decode_pal_rle_tile
                            // does — it is protocol state, and the GPU needs
                            // the table to decode against.
                            if let Some(upsert) = &validated.palette_upsert {
                                let slot = &mut self.palettes[validated.palette_id as usize];
                                let mut reported =
                                    Vec::with_capacity(validated.count as usize);
                                for i in 0..validated.count as usize {
                                    let bgra = [
                                        upsert[i * 4],
                                        upsert[i * 4 + 1],
                                        upsert[i * 4 + 2],
                                        upsert[i * 4 + 3],
                                    ];
                                    slot[i] = bgra;
                                    reported.push(bgra);
                                }
                                self.palette_shadow
                                    .put(validated.palette_id, validated.count);
                                events.push(Event::PaletteUpdated {
                                    palette_id: validated.palette_id,
                                    colors: reported,
                                });
                            }
                            events.push(Event::TilePayload {
                                frame_seq,
                                tile_x: tx,
                                tile_y: ty,
                                pass_idx: asm.pass,
                                generation: asm.generation,
                                codec: Codec::PalRle,
                                payload,
                            });
                        }
                        Err(code) => {
                            if let Some(msg) = self.decode_error_batcher.report(
                                Codec::PalRle,
                                tx,
                                ty,
                                code,
                                now_us,
                            ) {
                                self.outbox.push_back(PollOutput::Stream(msg));
                            }
                        }
                    }
                    return;
                }
                // ... existing Decoded path unchanged below ...
            }
```

Import `prevalidate_pal_rle` alongside the existing `decode_pal_rle_tile` import. Keep the existing `Decoded` body exactly as it is — do not refactor it while adding the branch, or a regression there will be indistinguishable from the new code.

Check the error-reporting line against the existing `Err(code)` arm in this file and mirror it exactly; the `self.outbox.push_back(...)` shape above is from the surrounding code but verify it.

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p ghostframe-client-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-core/src/reassembly.rs
git commit -m "feat(client-core): payload delivery for PalRle, with palette reporting"
```

---

### Task 7: Payload mode for Cdf53

Under `Decoded`, the Cdf53 arm prevalidates, updates coverage, queues pass NACKs, ACKs, and calls `cdf53_tile_state.integrate` to accumulate passes into pixels. Under `Payload`, everything except `integrate` must still happen — the GPU accumulates in its own buffers (`cdf53_integrate.wgsl`).

**Files:**
- Modify: `ghostframe-client-core/src/reassembly.rs` (the `Codec::Cdf53` arm)

- [ ] **Step 1: Write the failing test**

```rust
    /// Payload mode must keep every protocol side effect of a Cdf53 pass —
    /// prevalidation, coverage, the deferred ACK — and drop only the CPU
    /// accumulation, which the GPU does instead. Losing the ACK would make
    /// the server retransmit every pass forever.
    #[test]
    fn cdf53_in_payload_mode_keeps_the_ack_and_skips_integrate() {
        let pass0 = cdf53_pass_payload_for_test(0);

        let (events, outputs) = drive_one_tile_with_outputs(
            TileDelivery::Payload,
            ghostframe_protocol::protocol::Codec::Cdf53,
            &pass0,
        );

        assert!(
            events.iter().any(|e| matches!(e, Event::TilePayload { codec, .. }
                if *codec == ghostframe_protocol::protocol::Codec::Cdf53)),
            "expected a Cdf53 TilePayload, got {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::TileReady { .. })),
            "Payload mode must not emit TileReady"
        );
        assert!(
            !outputs.is_empty(),
            "the deferred ACK must still be produced — without it the server \
             retransmits this pass indefinitely"
        );
    }
```

`cdf53_pass_payload_for_test(pass_idx)` builds one valid pass payload. Derive it from `ghostframe_protocol::codec::cdf53::{forward, encode_passes}` over a fixed tile, the same way `ghostframe-e2e/src/harness/scene_tiles.rs` does. `drive_one_tile_with_outputs` is `drive_one_tile` also returning the `PollOutput`s drained from the core — extend the helper rather than duplicating it.

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test -p ghostframe-client-core --test tile_delivery cdf53_in_payload_mode`
Expected: FAIL — `TileReady` emitted instead of `TilePayload`.

- [ ] **Step 3: Implement**

In the `Codec::Cdf53` arm's `Ok(pre)` branch, replace the integrate-and-emit pair with a delivery-mode switch, leaving everything around it — coverage, NACK queueing, the deferred ACK — untouched:

```rust
                    Ok(pre) => {
                        match self.tile_delivery {
                            TileDelivery::Decoded => {
                                let rgba = self.cdf53_tile_state.integrate(tx, ty, &pre);
                                events.push(Event::TileReady {
                                    frame_seq,
                                    tile_x: tx,
                                    tile_y: ty,
                                    rgba,
                                });
                            }
                            TileDelivery::Payload => {
                                // No CPU accumulation: cdf53_integrate.wgsl
                                // accumulates passes in GPU buffers, keyed by
                                // its own tileGen buffer.
                                events.push(Event::TilePayload {
                                    frame_seq,
                                    tile_x: tx,
                                    tile_y: ty,
                                    pass_idx: asm.pass,
                                    generation: asm.generation,
                                    codec: Codec::Cdf53,
                                    payload: payload.clone(),
                                });
                            }
                        }
                        // ... existing deferred-ACK code unchanged ...
                    }
```

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p ghostframe-client-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-core/src/reassembly.rs
git commit -m "feat(client-core): payload delivery for Cdf53"
```

---

### Task 8: Prove the default path did not move

Five tasks have touched the dispatch that every existing consumer depends on. This task exists to show `Decoded` is byte-identical to before.

**Files:** none — verification only.

- [ ] **Step 1: Full workspace verification**

```bash
export TMPDIR=/home/cedric/.cache/ghostframe-tmp
cargo test -p ghostframe-client-core
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-lib --test bwe_bench
cargo test -p ghostframe-e2e --test netsim
cargo test -p ghostframe-e2e --test netsim_pump
cargo test -p ghostframe-e2e --test scene_tiles
cargo test -p ghostframe-e2e --test framebuffer
cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check --all
```

Expected, all unchanged from before this plan: lib 390, bwe_bench 3, netsim 9, netsim_pump 5, scene_tiles 7, framebuffer 9, browserless_runner 10. `ghostframe-client-core` will have grown by the tests added here — report the number.

- [ ] **Step 2: Mutation-check the default**

Temporarily change `TileDelivery`'s `#[default]` from `Decoded` to `Payload`
and re-run **`cargo test -p ghostframe-e2e --test browserless_runner --
--test-threads=1`**.

Expected: **failures**. `ghostframe-client-net/src/lib.rs:102-106` builds its
`ClientConfig` with `..Default::default()`, so the flipped default reaches the
`ClientCore` inside `ClientNet`; the scenes then receive `TilePayload` instead
of `TileReady` and their pixel assertions fail.

**Do not expect `--test framebuffer` to fail.** It never constructs a
`ClientCore` — it drives `FrameBuffer` directly — so the default cannot reach
it. Task 3 recorded that baseline: under a flipped default, `framebuffer`
passed 9/9. It should still pass now, and that is correct, not a gap.

If `browserless_runner` still passes, the default is not reaching its consumer
and `TileDelivery` is decorative — stop and report. Restore the `#[default]`
and confirm green again.

- [ ] **Step 3: Commit nothing, report**

This task produces no commit. Report the numbers and both mutation outcomes.

---

## Done when

- Every web-client build in CI goes through `_build-web-client`; `grep -rn "npm run build" .github/workflows/` returns only the action itself.
- `TileDelivery::Payload` emits `TilePayload` for all four tile codecs, plus `PaletteUpdated` for bundled PalRle.
- `TileDelivery::Decoded` is the default and demonstrably unchanged.
- No browser code has been touched.

## Two CI findings, recorded not fixed

Both surfaced while classifying the build sites. Neither is in this plan's
scope; both want a decision.

**The cross-workflow artifact download never works.** `ci.yml` uploads
`web-client-dist`; `e2e.yml` downloads it with
`run-id: ${{ github.run_id }}`. But `github.run_id` is *e2e.yml's own* run,
and the artifact belongs to a **different workflow's** run, so the download
finds nothing. `continue-on-error: true` hides the failure and the fallback
build runs instead.

Confirmed on a completed run: the step `Fallback web-client build if artifact
missing` reports conclusion **`success`**, not `skipped` — meaning
`hashFiles('ghostframe-web-client/dist/**') == ''` was true and the local
build ran. Every e2e run rebuilds the web client despite the optimisation.

Fixing it properly means either a cross-workflow artifact lookup by branch and
workflow name, or merging the producing job into `e2e.yml`. Worth doing —
it is a whole npm install and build per e2e run — but it is a CI
restructuring, not part of the wasm groundwork.

**`npx tsc --noEmit` at `ci.yml` ~137 is redundant.** `npm run build` is
`tsc && vite build` (`ghostframe-web-client/package.json:7`), and the extra
invocation uses the same `tsconfig.json`, so it re-checks exactly what the
build already checked. `--noEmit` changes output, not checking. Removing it
would let that site use the composite action like the other eight. Left alone
here because deleting a type-check is a behaviour change, not a refactor.

## Explicitly out of scope

The `ghostframe-client-wasm` crate, `wasm-bindgen`, the retargeted vitest suites, adding the Rust toolchain to the composite action, and any change under `ghostframe-web-client/src/`. Those are steps 3 and 4 of the spec and get their own plan. In particular, do **not** add `wasm-pack` to the composite action in this plan — the action landing unchanged is what makes it a safe refactor.
