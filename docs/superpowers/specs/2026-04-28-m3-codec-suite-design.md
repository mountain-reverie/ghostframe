# M3 Codec Suite + Refinement — Umbrella Design

**Date:** 2026-04-28
**Status:** Design approved
**Source spec:** `docs/specs/ghostframe-initial-spec.md` (§4 Tile Engine, §5 Encoding Pipeline, §12 Milestones)
**Predecessors:** M0–M3 plan (`2026-04-11-implementation-plan-m0-m3-design.md`), M2 zero-copy (`2026-04-23-m2-zero-copy-gpu-pipeline-design.md`), bench harness (`2026-04-27-codec-bench-skeleton.md`)

---

## Context

After M2, the pipeline emits full-frame H.264 over DMA-BUF zero-copy with GPU-resident dirty detection. The `Codec` enum already reserves all six variants (`Skip`, `H264`, `PalRle`, `Bc1`, `Solid`, `Raw`, `Cdf53`), the protocol fragmenter handles tile and frame datagrams, and the bench harness skeleton is in place. The classifier (`TileMetrics`, `CodecState`, two-axis rules, hysteresis), the four non-H.264 codecs, and progressive refinement do not yet exist — they are M3.

This design replaces the original "single big M3" structure with a **six-phase split**, each phase independently shippable and testable end-to-end. The split was chosen to keep each cycle to a single brainstorm → plan → implement loop and to deliver visible value at every step.

---

## Architecture

Three central ideas hold M3 together; the rest is build-out.

### 1. Frame-mode switch (no mixing)

Every emitted frame is either an **H.264 frame** (full-frame VA-API encode, current M2 path) OR a **tile-codec frame** (per-tile encodings sent as tile datagrams). Never both for the same `frame_seq`. The classifier decides per frame which mode wins.

Re-entry into H.264 mode forces an I-frame because temporal context was lost while in tile-codec mode; intra-refresh resumes from there.

This decision retires the prior hypothetical "mixed per-frame" architecture: H.264 always covers the whole frame when active, and per-tile codecs handle the whole frame when active. The encoder never fights another encoder over the same pixels.

### 2. Cost-aware classifier

Per frame:
- Run the per-tile classification rules (§4.2 of the source spec) to get *tentative* per-tile codec assignments.
- Sum estimated encode + bandwidth costs of those assignments.
- Compare to estimated H.264-frame cost.
- Two trigger paths to switch into H.264 mode:
  - **Cost path:** total tile-codec cost exceeds H.264 cost × `ENTER_FACTOR`, sustained N frames.
  - **Sustained-motion fast-path:** count of tiles tentatively classified `H264` exceeds `MOTION_TILE_THRESHOLD`, sustained N frames.
- Hysteresis on both directions to prevent flapping.

Cost constants are **hardcoded** from spec assumptions in Phase M3.0. Adaptive online profiling that updates constants from observed encode times is recorded as future work.

### 3. Codec-agnostic scheduler with batched datagram ACKs

A single round-robin scheduler dispatches *all* tile-codec emissions, treating Solid as a 1-pass codec, PalRLE as a 1-pass codec, and CDF 5/3 as an N-pass codec. Each pass carries `(tile_x, tile_y, gen, pass_idx)`.

Client batches ACKs into unreliable datagrams (≤5 ms or 64 entries, whichever first). Server uses ACKs to drive its retry loop and round-robin queue eligibility. Generation counter on each tile invalidates pending retries when content changes.

The scheduler is built in M3.1 alongside the simplest possible codec (Solid) and reused unchanged for every subsequent codec.

### Cleanup arising from this architecture

The existing per-tile `H264VaapiEncoder` in `encoder/h264_vaapi.rs` was preserved by the M2 plan as "dead code for M3 lossless tile codecs". With the frame-mode switch, per-tile H.264 is no longer used. M3.0 removes it; only `FullFrameEncoder` remains.

---

## Phase Breakdown

| Phase | Theme | Key deliverables | E2E gate |
|---|---|---|---|
| **M3.0** | Frame-mode switch + classifier scaffolding | `TileMetrics`, `CodecState`, classifier with cost model (hardcoded constants), mode-switch logic in `io_bridge`, hysteresis. Per-tile emission stays Raw. Remove old per-tile H264VaapiEncoder. | `e2e_mode_switch` (new): synthetic content drives mode A→B→A; protocol-layer assertion confirms correct frame discriminator on each `frame_seq`. Unit tests cover boundary cases of the cost model. |
| **M3.1** | Solid codec + scheduler/ACK infrastructure | Solid encoder, generation counter per tile, round-robin scheduler, batched ACK datagram protocol, retry policy, web-client Solid render. | `e2e_solid_color` strengthens: solid regions encode as 4 bytes/tile and survive simulated packet loss via retries. `e2e_ack_loss` (new): drop ACKs for 2 s, assert no retry storm, eventual delivery. |
| **M3.2** | PalRLE + palette table | Shared palette table, LRU eviction, PalRLE encoder, classifier rule, WASM PalRLE decoder, palette piggybacking on tile datagrams. | `e2e_text_clarity` extends: text uses `Codec::PalRle`, payload drops vs Raw, decoded SSIM = 1.0. `e2e_palette_eviction` (new). |
| **M3.3** | CDF 5/3 + multi-pass progressive refinement | Vulkan compute forward transform, bit-plane extraction, multi-pass scheduler, WebGPU compute inverse with bit-plane accumulation. | `e2e_progressive_refinement` (new): SSIM on idle region monotonically increases over 5 s, hits 1.0 at lossless. `e2e_refinement_cancel` (new). |
| **M3.4** | BC1 (conditional on M3.5 results) | Vulkan compute BC1 encode, WebGPU compute BC1 decode, classifier rule. **May be skipped** if M3.5 bench shows CDF 5/3 dominates BC1 across all content classes. | `e2e_bc1_gradient` (new, only if BC1 lands). |
| **M3.5** | Bench publication + threshold tuning + BC1 fate decision | Run full bench suite on real GPU, publish markdown table per codec × content class × LZ4 on/off, recompute classifier cost-model constants from real numbers, decide BC1's fate. | Bench results committed to repo; classifier constants updated; either M3.4 launches or BC1 is removed. |

**Ordering rationale:** the bench harness already exists, but per-codec `BenchEncoder` impls only land as each codec lands. M3.5 is naturally last because that's when all codecs are present to benchmark. Spot-checking partial benches at intermediate phases is allowed but does not block phase completion.

**Prerequisite gate per codec phase (M3.2, M3.3, M3.4):** each requires its own per-codec brainstorm and design spec at `docs/superpowers/specs/YYYY-MM-DD-<codec>-codec-design.md` *before* the corresponding writing-plans run. M3.0 and M3.1 proceed straight from this umbrella to writing-plans because their designs are complete here. M3.5 needs no further brainstorm.

---

## Phase M3.0 — Frame-mode switch + classifier scaffolding

### New types in `ghostframe-lib/src/tile/mod.rs`

```rust
pub struct TileMetrics {
    pub change_freq_hz: f32,        // EMA, alpha = 0.1
    pub change_magnitude: f32,      // SAD / area, normalized to [0, 1]
    pub unique_colors: u16,         // hash-based estimate from GPU compute output (sentinel in M3.0)
    pub edge_density: f32,          // gradient pixel fraction (sentinel in M3.0)
    pub idle_frames: u32,
    pub codec_state: CodecState,
}

pub enum CodecState {
    Skip,
    H264 { frames_in_h264: u32 },
    Bc1,
    PalRle { palette_id: u8 },
    Solid,
    Cdf53 { passes_sent: u8, max_passes: u8 },
    PixelPerfect,
}

pub enum FrameMode { H264, TileCodec }
```

**Storage:** per-tile state lives in a `Vec<TileMetrics>` of length `cols × rows`, parallel to `DirtyTracker.prev_tiles`, recreated on `resize()`. Indexed by `tile_y * cols + tile_x`.

**M3.0 sentinel policy for GPU-derived metrics:** `unique_colors` and `edge_density` require GPU compute support that doesn't land until M3.3. In M3.0 they are populated with sentinels — `unique_colors = u16::MAX`, `edge_density = f32::NAN` — and any classifier rule that consults them treats the sentinel as "unknown, do not match". Unit tests inject concrete values directly into `TileMetrics` to exercise rules. `change_freq_hz` (EMA from dirty-tile observation) and `idle_frames` (incremented per frame on no-change, reset to 0 on dirty) are populated normally because they only require the existing dirty detector.

### New module `ghostframe-lib/src/tile/classifier.rs`

```rust
pub struct CostModel {
    pub solid_us: f32,         // ~0.5 µs   (4-byte memcpy)
    pub palrle_us: f32,        // ~5 µs     (nibble-pack 1024 px)
    pub bc1_us: f32,           // ~50 µs    (Vulkan compute, dispatched)
    pub cdf53_us: f32,         // ~50 µs    (Vulkan compute, dispatched)
    pub raw_us: f32,           // ~1 µs     (memcpy)
    pub h264_frame_us: f32,    // ~3000 µs  (VA-API full-frame encode @ 1080p)
    pub bytes_per_us: f32,     // bandwidth weighting; see "Bandwidth source" below
}

pub struct Classifier {
    cost: CostModel,
    enter_factor: f32,         // default 1.3
    exit_factor: f32,          // default 0.6
    motion_tile_threshold: f32,// default 0.20 (20% of dirty tiles)
    motion_tile_min_absolute: u32, // default 8 — absolute floor; both fraction AND count must hold
    enter_sustain_frames: u32, // default 3
    exit_sustain_frames: u32,  // default 30
    state: ClassifierHysteresis,
}

impl Classifier {
    /// Pure function: per-tile rule application from §4.2 with §4.3 hysteresis.
    pub fn classify_tile(metrics: &TileMetrics, prev: &CodecState) -> CodecState;

    /// Cost-aware mode decision; mutates internal hysteresis counters.
    pub fn decide_frame_mode(
        &mut self,
        per_tile_states: &[CodecState],
        per_tile_metrics: &[TileMetrics],
        prev_mode: FrameMode,
    ) -> FrameMode;
}
```

The values in `CostModel` are placeholders rounded from the spec's GPU compute claims. M3.5 retunes them from real measurements on actual hardware.

**Bandwidth source for `bytes_per_us`:** source spec §6.5 specifies a two-layer bandwidth estimator (QUIC congestion controller as ceiling + app-level estimator from `ReceiverFeedback` loss/throughput). That machinery is not yet implemented (M4+ work). M3.0 uses a hardcoded placeholder consistent with a typical home-LAN link (e.g. 100 Mbps → ~12 bytes/µs). The classifier reads `bytes_per_us` through a single accessor so the wire-up to the §6.5 estimator is a one-call change once it lands.

### Spec divergence on enter/exit (intentional)

Source spec §4.3 expresses H.264 entry/exit as metric-based rules (`change_freq > 15 Hz AND change_magnitude > 0.3`, sustained 10 frames to enter; `freq < 5 Hz OR magnitude < 0.1`, sustained 30 to exit). The M3.0 design replaces this with a cost-based decision plus a sustained-motion fast-path on tile-classification counts. Rationale: the cost path generalizes — it adapts as new codecs land and as `bytes_per_us` becomes link-aware — whereas the metric thresholds were a hand-tuned proxy for cost. The fast-path preserves the spirit of "lots of moving tiles → H.264".

Post-M3 evolution (recorded in Future work below): augment the cost decision with a **trend signal** — current frame cost combined with a short rolling window of past costs and per-frame H.264-tile fractions — so the classifier predicts when a stream is becoming video-ish before the steady-state cost rule catches up. This adds frequency awareness without re-introducing the hand-tuned thresholds.

### Mode-switch rule

```
tile_codec_cost = Σ cost_us(tentative_codec_for_tile) over dirty tiles
                + bytes_per_us × Σ estimated_bytes(tentative_codec_for_tile)
h264_cost       = h264_frame_us
                + bytes_per_us × estimated_h264_bytes_per_frame

enter_h264 if   (tile_codec_cost > h264_cost × ENTER_FACTOR)
                sustained ENTER_SUSTAIN_FRAMES
             OR (#tiles_classified_H264 >= MOTION_TILE_MIN_ABSOLUTE
                 AND #tiles_classified_H264 / max(#dirty_tiles, 1) > MOTION_TILE_THRESHOLD)
                sustained ENTER_SUSTAIN_FRAMES
exit_h264  if   (tile_codec_cost < h264_cost × EXIT_FACTOR)
                sustained EXIT_SUSTAIN_FRAMES
deadband   otherwise → keep prev_mode
```

The absolute-count floor on the fast-path prevents a single H.264-classified tile out of one or two dirty tiles (cursor blink, single-pixel UI tick) from promoting the whole frame to H.264. If `#dirty_tiles == 0` the function is short-circuited at the call site — `decide_frame_mode` is only consulted when there is at least one dirty tile to dispatch.

### `io_bridge` changes

- `process_frame_gpu` consults the classifier to pick `FrameMode`. Both modes are supported on this path.
- `process_frame_cpu` **always** stays in `TileCodec` mode emitting `Codec::Raw`. The CPU fallback path does not gain a full-frame H.264 emitter in M3.0 — wiring `FullFrameEncoder` (libx264 backend) into the CPU path is recorded as future work; today's CPU path is a degraded fallback used only when the GPU pipeline is unavailable, and emitting Raw matches that reality.
- `H264` mode (GPU path only) → existing full-frame VA-API path. On *re-entry* (prev was `TileCodec`), force an IDR so the client has a fresh anchor.
- `TileCodec` mode → for each dirty tile, emit one tile datagram. In Phase 0 the codec is always `Codec::Raw` because no per-tile codecs exist yet; later phases swap in real codecs per `CodecState`.

**Force-IDR API addition.** `FullFrameEncoder.encode_nv12_buffer` currently computes `force_idr` internally from `pts % FULL_FRAME_GOP == 0` (h264_vaapi.rs:720). M3.0 adds a `request_keyframe()` method on `FullFrameEncoder` that latches a one-shot flag OR'd into the next encode's `force_idr`. The mode-switch logic calls this on every `TileCodec → H264` transition.

### Cleanup

- Remove the per-tile `H264VaapiEncoder` and the per-tile encoder map from `io_bridge`.
- Keep `FullFrameEncoder` only.

### Testing

- Unit tests on `classify_tile` (rule table) using `proptest_strategies::TileMetrics` (introduce strategy alongside existing `frame_packed`).
- Unit tests on `decide_frame_mode` boundary cases — cost just above/below threshold; hysteresis enter/exit timing; `MOTION_TILE_MIN_ABSOLUTE` floor (single H.264-classified tile out of one or two dirty tiles must NOT trip the fast-path).
- Re-enable the M3 classifier proptest invariants applicable to M3.0 — **C1, C2, C4, C5** — at `tile/tests_proptest.rs:289`. **C3** (Solid) defers to M3.1 (which ships the Solid encoder), **C6** (refinement traversal) defers to M3.3 (which ships refinement). Both are tracked in their respective phase sections below.
- E2E `e2e_mode_switch` (new): test pattern alternates between fully-static and fully-moving content; protocol-layer assertion that `frame_seq` N is one mode or the other (never both) and that mode flips happen at the right boundaries with the right hysteresis.

**Test-pattern dependency:** `e2e_mode_switch` requires a new mode flag in `ghostframe-test-pattern` (e.g. `--mode-switch-cycle SECS`) that drives synthetic content alternating between fully-static and fully-moving regions on a known cadence. This is part of M3.0 deliverables.

### Out of scope for M3.0

- Real Solid/PalRLE/BC1/CDF53 encoders (placeholders only — emission is Raw).
- Round-robin scheduler (M3.1).
- ACK protocol (M3.1).
- Refinement (M3.3).

---

## Phase M3.1 — Solid + scheduler/ACK infrastructure

This is the **infrastructure phase**. Solid is the trivial codec we hang it on; the bulk of the work is the scheduler, the ACK protocol, and the retry loop.

### New module `ghostframe-lib/src/encoder/solid.rs`

```rust
pub fn encode_solid(tile: &[u8]) -> [u8; 4];
pub fn decode_solid(payload: &[u8]) -> Result<[u8; 4], DecodeError>;
```

### New module `ghostframe-lib/src/transport/scheduler.rs`

```rust
pub enum WorkState { Pending, InFlight, Acked, Superseded }

pub struct TileWork {
    pub tile_x: u8,
    pub tile_y: u8,
    pub generation: u8,        // 4 bits effective (see Wire Protocol)
    pub pass_idx: u8,          // 4 bits effective; 0 for single-pass codecs
    pub total_passes: u8,
    pub codec: Codec,
    pub payload: Vec<u8>,
    pub queued_at: Instant,
    pub last_sent_at: Option<Instant>,
    pub state: WorkState,
}

pub struct Scheduler {
    generations: Vec<u8>,           // length = cols × rows
    queue: SchedulerQueue,
    cursor: usize,                  // round-robin position
    rtt: Duration,                  // from QUIC stats
    refinement_fraction: f32,       // default 0.2 (M3.3 adapts)
}

impl Scheduler {
    pub fn enqueue(&mut self, work: TileWork);
    pub fn bump_generation(&mut self, tile_x: u8, tile_y: u8) -> u8;
    pub fn tick(&mut self, budget_bytes: usize) -> Vec<TileWork>;
    pub fn on_ack(&mut self, tile_x: u8, tile_y: u8, gen: u8, pass: u8);
}
```

For Phase M3.1 every Solid tile is a single 1-pass work item; the scheduler still goes through the full enqueue → tick → ack cycle so the plumbing is exercised end-to-end.

`bump_generation` is invoked from `process_frame` whenever the dirty-tile detector reports a tile changed. Wrap-around at 16 is safe because the same call clears in-flight entries for the previous generation; no stale gen value can be in flight when wrap reuses it.

### ACK datagram format

Added to `transport/protocol.rs`:

```
ACK_BATCH_MSG_TYPE = 0x02

[0]      message_type = 0x02
[1]      count: u8           (1..64)
[2..]    count × 4 bytes:
            [0] tile_x: u8
            [1] tile_y: u8
            [2] generation: u4 << 4 | pass: u4
            [3] reserved: u8
```

Max size: 1 + 1 + 64 × 4 = 258 bytes — well under the 1200-byte MTU budget.

### Web client changes

- New `ack.ts` — `AckBatcher` class. Buffers ACK entries; flushes on every datagram-receive callback if `≥64` entries OR `≥5 ms` since last flush.
- `decoder.ts` — Solid codec decode: 4 bytes → fill 32 × 32 region.
- `renderer.ts` — `drawSolidTile(x, y, bgra)` via `ctx.fillStyle / ctx.fillRect`.
- `main.ts` — wire `AckBatcher` into the datagram receive path; one unidirectional WebTransport datagram per flush.

### `io_bridge` changes

- New tile-codec emission path: classifier → for each dirty tile → if `CodecState::Solid` then `enqueue(TileWork{ codec: Solid, payload: encode_solid(tile), ... })`. Other states fall back to Raw via the same enqueue path until later phases land their codecs.
- Per scheduler `tick()`, fragment each returned `TileWork.payload` via `fragment_tile()` and send.
- New ACK receive path: parse `ACK_BATCH_MSG_TYPE` datagrams → `scheduler.on_ack()` for each entry.
- Retry loop: scheduler internally promotes `InFlight` → `Pending` after `2 × RTT`.

### Testing

- Unit tests on `Scheduler`: round-robin fairness, retry timing, generation invalidation cancels stale work, budget enforcement.
- Unit tests on `AckBatcher`: flush-on-count and flush-on-time, encoding/decoding roundtrip.
- **Re-enable proptest invariant C3** (deferred from M3.0) — `unique_colors == 1 ⇒ CodecState::Solid` — now that the Solid encoder backs the classifier rule end-to-end.
- E2E `e2e_solid_color` (extend existing): assert solid regions emit `Codec::Solid` 4-byte payloads, render correctly, survive simulated 5 % datagram loss via retries.
- E2E `e2e_ack_loss` (new): drop 100 % of ACK datagrams for 2 s; assert the server keeps retrying, no retry storm (rate-limited by RTT-based retry interval and budget), and tiles eventually render once ACKs flow again.

### Wire-protocol additions deferred from M3.0

The `(gen << 4) | pass_idx` packing in `TileHeader` byte `[3]` (see Wire Protocol Additions section) lands here in M3.1 alongside the scheduler, since M3.0 does not use generations or passes. M2 sites continue to set `generation = 0`; the byte split is backward-compatible.

### Out of scope for M3.1

- The lossy codecs (PalRLE, BC1, CDF 5/3) — fall back to Raw.
- Multi-pass scheduling.
- Refinement-bandwidth adaptation under sustained congestion (M3.3).

---

## Phase M3.2 — PalRLE + palette table

**Prerequisite:** per-codec brainstorm and design spec at `docs/superpowers/specs/YYYY-MM-DD-palrle-codec-design.md` before writing-plans is invoked for this phase. The notes here are the scope envelope, not the implementation contract.

The user-visible win: text becomes crisp and small. The implementation centerpiece is the **shared palette table** with reference counting and inline piggyback delivery.

### New module `ghostframe-lib/src/encoder/pal_rle.rs`

```rust
pub struct PaletteEntry {
    pub colors: [[u8; 4]; 16],   // BGRA
    pub count: u8,                // active entries (1..=16)
    pub ref_count: u32,
}

pub enum SlotState { Empty, Held, FreeButCached }

pub struct PaletteTable {
    entries: [Option<PaletteEntry>; 256],
    slot_state: [SlotState; 256],
    free_lru: VecDeque<u8>,
}

impl PaletteTable {
    /// Returns palette_id of any slot (Held or FreeButCached) whose colors
    /// contain all of `tile_colors`.
    pub fn find_matching(&self, tile_colors: &[[u8; 4]]) -> Option<u8>;

    /// Allocates a slot for new content; reuses the oldest FreeButCached
    /// slot if available (overwriting bytes), else extends into Empty space.
    /// Returns `None` if every slot is Held.
    pub fn allocate(&mut self, tile_colors: &[[u8; 4]]) -> Option<u8>;

    pub fn acquire(&mut self, palette_id: u8);
    pub fn release(&mut self, palette_id: u8);
}

pub fn encode_pal_rle(tile: &[u8], palette: &PaletteEntry) -> Vec<u8>;
pub fn decode_pal_rle(payload: &[u8], palette: &PaletteEntry) -> Vec<u8>;
```

### Palette delivery via tile piggyback

There is no separate `PaletteUpdate` control message. Palettes ride inline in the tile datagrams that use them.

Per client session:

```rust
struct ClientPaletteState {
    delivered: BitSet<256>,   // palette_id → has been ACKed via any tile
}
```

When the server schedules a tile with `Codec::PalRle`:
- If `delivered[palette_id]` is set → emit **thin** PalRle payload (no bundle).
- Else → emit **bundled** PalRle payload (palette colors inline + RLE bytes).

`delivered[id]` updates:
- `acquire()` (ref_count++): no change.
- `release()` (ref_count → 0): no change. Slot moves to `FreeButCached`, pushed onto `free_lru`. The client's cached copy remains valid as long as the slot bytes are preserved.
- `find_matching()` searches both `Held` and `FreeButCached` slots — a tile reusing exact colors hits the cached slot, increments ref_count, and inherits the existing `delivered` state.
- `allocate()` reassigning a `FreeButCached` slot with **new content** → **clear `delivered[id]`** because the slot bytes are about to change.

This avoids unnecessary palette retransmits when content briefly flips away and back; only genuine slot repurposing forces a resend.

When all tiles using a palette are superseded mid-flight, surviving tiles using the same palette (in their own next scheduler ticks) continue to bundle it until one of them is ACKed. The retry mechanism is naturally correct: a palette is delivered when *any* tile carrying it is ACKed.

### PalRle payload format

Codec-specific framing inside the tile datagram payload:

```
[0]        flags: u8        (bit 0 = palette_bundled, bits 1..7 reserved)
[1]        palette_id: u8
[2..]      if palette_bundled:
              [count: u8]
              [count × 4 bytes BGRA]
[then..]   nibble-packed RLE bytes (high nibble = color index, low = run-1)
```

Bundled overhead: 1 + 16 × 4 + 1 = 66 bytes max (smaller palettes carry less).

### Web client changes

- New `palette.ts` — `PaletteCache` keyed by `palette_id`. Receives bundled palettes from incoming PalRle payloads, applies. Decoder looks up palette by ID per tile.
- WASM module `ghostframe-web-client/wasm/src/lib.rs` — first WASM in the project. `decode_pal_rle(payload, palette) -> Uint8ClampedArray`. Justification: nibble unpacking + palette lookup is hot per-frame work; WASM keeps it off the JS critical path.
- `renderer.ts` — `drawPalRleTile(x, y, bgraData)` via `putImageData()`.

### Classifier rule additions

In `tile/classifier.rs`:
- `unique_colors == 1` → Solid.
- `unique_colors ≤ 16 AND palette_table.allocate_or_find_succeeds(...)` → PalRle.
- Allocation failure (every slot is Held) → fall back to Raw at the time M3.2 ships. M3.3 updates this fallback to `Cdf53` once that codec is available. (One-line rule change in the classifier; tracked as part of M3.3's deliverables.)

The classifier needs a read-only handle to the palette table to test feasibility. Pass it via `Classifier::classify_tile(metrics, prev_state, palette_table_view)`.

### Cost-model additions

- Real `palrle_us` is set by M3.5; this phase ships with the placeholder.
- `palrle_bytes_estimate(unique_colors, edge_density)` — needed by the cost-aware mode switch. Use the spec's "100–200 bytes for typical text" as the typical, with edge density modulating between 64 (single solid run) and 1024 (worst-case high-detail).

### Testing

- Unit tests on `PaletteTable`: `allocate / acquire / release` ordering, LRU eviction picks oldest free slot, full-table allocation fails cleanly, `find_matching` hits both Held and FreeButCached slots, `delivered` clears only on bytes-change reallocation.
- Property tests: `encode_pal_rle → decode_pal_rle` preserves pixels exactly (palettized RLE is lossless).
- E2E `e2e_text_clarity` (existing) extends: text region uses `Codec::PalRle`, payload is significantly smaller than Raw equivalent, decoded pixels match source exactly (SSIM = 1.0).
- E2E `e2e_palette_eviction` (new): force the palette table close to capacity by drawing many distinct text regions; verify capacity reclamation when regions disappear; verify reusing the same colors hits the cached slot without a re-bundle.

### Out of scope for M3.2

- BC1 and CDF 5/3 (M3.3+).

---

## Phase M3.3 — CDF 5/3 + multi-pass progressive refinement

**Prerequisite:** per-codec brainstorm and design spec at `docs/superpowers/specs/YYYY-MM-DD-cdf53-codec-design.md` before writing-plans. This phase touches both Vulkan and WebGPU compute and has subtle bit-plane semantics — too much to lock down in the umbrella. The notes below are the scope envelope.

### Server-side encode (`ghostframe-lib/src/encoder/cdf53.rs`)

- Vulkan compute shader at `capture/shaders/cdf53_forward.comp`.
- Per 32 × 32 tile: integer CDF 5/3 forward wavelet transform, 3 levels of decomposition → 1024 integer coefficients per channel.
- Bit-plane extraction: from MSB toward LSB, each pass emits one bit-plane (sign + magnitude bit) for all coefficients, run-length-encoded.
- Coefficient layout, exact pass count, and packing scheme finalized in the per-codec brainstorm. Initial estimate from §4.4: 9 bit-planes per channel, ~140 bytes/pass, ~1.0–1.3 KB total to lossless.

### Server-side multi-pass scheduler integration

- `Scheduler::enqueue_refinement(tile_x, tile_y, gen, all_passes: Vec<Vec<u8>>)` adds N work items in one call.
- Round-robin tick advances passes per tile in order: every tile sends pass 0 before any tile sends pass 1.
- New scheduler state: `refinement_bandwidth_fraction: f32` (default 0.2). When ACK delivery rate drops below 0.5 sustained 10 rounds → halve it; when delivery rate recovers above 0.8 → restore.
- `tick(budget_bytes)` partitions budget into "high-priority" (new dirty tiles) and "refinement" (multi-pass on idle tiles), per the §4.4 priority table.

### Refinement entry trigger

- `tile.idle_frames > 30` AND `tile.codec_state ∈ { Bc1, PalRle, Solid }` (i.e. tile is currently displayed with a lossy or near-lossy snapshot) → run forward CDF 5/3, generate all bit-planes, enqueue all passes, transition `CodecState::Cdf53 { passes_sent: 0, max_passes: N }`.
- All passes ACKed → transition `CodecState::PixelPerfect`.
- Tile content changes during refinement → `bump_generation`; scheduler cancels remaining passes for old gen.

### Client-side decode

- Inverse transform implemented either as WASM (CPU) or WebGPU compute shader at `ghostframe-web-client/shaders/cdf53_inverse.wgsl`. Choice deferred to the per-codec brainstorm. The e2e gate ("SSIM monotonically increases to 1.0") requires correctness, not GPU acceleration; WASM is sufficient for first landing and WebGPU is a perf optimization that may come in a follow-up.
- Per-tile state: accumulating coefficient buffer across passes.
- WASM extension: `apply_pass(tile_x, tile_y, gen, pass_idx, payload)` → unpack bit-plane → OR into coefficient buffer → run inverse transform → blit to canvas.
- Client holds bit-planes per `(tile, gen)`; resets when gen advances.
- Out-of-order pass arrives: drop (don't ACK); server retries the earlier pass.
- Stale-gen pass arrives: drop (don't ACK).

### Cost-model additions

- `cdf53_us` placeholder, `cdf53_bytes_per_pass` (~140), `cdf53_max_passes` (~9). Real numbers from M3.5.

### Testing (envelope)

- Unit tests: forward + inverse CDF 5/3 on a fixed seed → exact reconstruction (lossless gate).
- Unit tests: bit-plane extraction → bit-plane reassembly → exact coefficients.
- Property test: `forward(t) → drop bottom-K bit-planes → inverse → SSIM` monotonically increases as K decreases (fewer dropped planes = higher SSIM).
- **Re-enable proptest invariant C6** (deferred from M3.0) — H.264 → idle transition must traverse a lossy intermediate codec (BC1 or PalRle, depending on what's available at this point) before refinement begins. C6 lands here because refinement is what closes the loop.
- E2E `e2e_progressive_refinement` (new): static-after-motion test pattern; SSIM measured at 1 s, 3 s, 5 s; assert monotone non-decreasing AND eventual 1.0.
- E2E `e2e_refinement_cancel` (new): start refinement, change content mid-stream, verify old-gen passes are dropped client-side and new-gen passes arrive.

### Out of scope for M3.3

- BC1 (M3.4, conditional).
- Multi-monitor (M4).

---

## Phase M3.4 — BC1 (conditional)

**Prerequisites (twofold):**
1. **M3.5 bench data is in.** This phase runs *after* M3.5 because the bench is the gate for whether BC1 is built at all.
2. **Per-codec brainstorm** at `docs/superpowers/specs/YYYY-MM-DD-bc1-codec-design.md` before writing-plans. Endpoint-selection algorithm choice (PCA vs min/max bounding) needs measured-quality justification.

### Phase gate (decided at end of M3.5)

- **BC1 lands** if the bench shows BC1 outperforms CDF 5/3 (lower encode time AND smaller compressed size at equivalent SSIM) for *any* content class.
- **BC1 is dropped** if CDF 5/3 dominates BC1 across *every* content class. Source spec §12 explicitly anticipates this outcome. In that case M3.4 closes with a documentation update and one short PR removing BC1 from `Codec` (or leaving the discriminant reserved — to be decided at the time).

### If BC1 lands — server side

- `ghostframe-lib/src/encoder/bc1.rs` and shader `capture/shaders/bc1_encode.comp`.
- 32 × 32 tile = 64 BC1 blocks of 4 × 4 pixels = 64 × 8 = 512 bytes/tile.
- Single-pass codec — slots into the existing scheduler with `total_passes: 1`.
- Classifier rule (per §4.2): `change_freq 5–15 Hz` AND not currently H.264 → BC1; or `change_freq > 15 Hz AND change_magnitude ≤ 0.3 AND unique_colors > 16` → BC1.

### If BC1 lands — client side

- Decode implemented either as WASM (CPU) or WebGPU compute shader at `ghostframe-web-client/shaders/bc1_decode.wgsl`. Choice deferred to the per-codec brainstorm; same rationale as CDF 5/3.
- 64 work items per tile (one per 4 × 4 block); decompresses to BGRA, blits to canvas region.

### Cost-model additions

- `bc1_us`, `bc1_bytes` (≈ 512 fixed) — placeholders Phase 0 used now backed by M3.5 measured data.

### Testing (envelope)

- Unit: encode → decode → SSIM vs reference for each content class. Threshold derived from spec's "good for photographic, not as good as H.264".
- E2E `e2e_bc1_gradient`: gradient region uses `Codec::Bc1`, payload is exactly 512 bytes/tile, decoded SSIM > threshold.

### Out of scope for M3.4

- BC1-specific progressive refinement. BC1 stays as a one-shot lossy snapshot. Idle tiles still escalate to CDF 5/3 for refinement to lossless per the M3.3 trigger rules.

---

## Phase M3.5 — Bench publication + threshold tuning + BC1 fate

This phase has **no new code paths**. It runs the existing bench harness on real hardware, analyzes the output, and uses the data to retune Phase 0's cost-model constants and decide BC1's fate. No further per-codec brainstorm needed.

### Inputs

- M3.0 cost-model constants in code.
- M3.1 Solid encoder + scheduler + ACK protocol.
- M3.2 PalRLE encoder + palette table.
- M3.3 CDF 5/3 forward encode + bit-plane extraction (the inverse client path is not benchmarked here).
- BC1 encoder needs a `BenchEncoder` impl too. Since BC1 hasn't been built yet, this phase **builds the BC1 encoder behind the bench harness only** — server emission stays gated until M3.4 lands. This is just enough BC1 to feed the bench.

### Deliverables

1. **`BenchEncoder` impls** for Solid, PalRLE, CDF 5/3, BC1 added to `ghostframe-lib/benches/codec_latency.rs`. The skeleton already specifies the trait + LZ4 wrapper — these are drop-in additions.
2. **Run the suite on real GPU hardware** (AMD/Intel VA-API + Vulkan):
   ```
   cargo bench -p ghostframe-lib --features gpu-bench,m3
   ```
3. **Compressed-size + quality side channels** alongside latency:
   - `compressed_bytes_per_class` per (codec, content class, lz4 on/off).
   - `ssim_per_class` per (codec, content class) — using a small Rust SSIM lib or a hand-rolled implementation against the original tile.
   - `bytes_to_lossless_per_class` for CDF 5/3 (sum of all bit-plane payloads to reach SSIM = 1.0).
4. **Results document** at `docs/specs/m3-codec-bench-results.md`:
   - Latency table (codec × class × lz4 on/off → µs and instructions).
   - Compressed-size table.
   - SSIM table.
   - Per-codec LZ4 break-even verdict (LZ4 ON / OFF / depends-on-class).
   - Per-class **recommended codec** (the cost-model winner).
   - **BC1 fate decision** with the dominance check from M3.4 prerequisite #1 explicit.
   - Bytes-to-lossless comparison: CDF 5/3 progressive total vs BC1 → PalRLE → Raw chain.
5. **Code changes from results:**
   - Update `tile/classifier.rs` `CostModel` constants from placeholders to measured values.
   - Update per-codec `lz4` flag default per content class from the break-even verdict.
   - If BC1 is dominated → open a follow-up "drop BC1" PR (or schedule it); update the spec's §4.2 rule table to remove BC1 references.
   - If BC1 wins anywhere → M3.4 unblocks.
6. **Re-run regression check.** After the constants update, re-run M3.0's `e2e_mode_switch` on a known fixture to confirm the mode boundary hasn't shifted in a way that breaks correctness. (Quantitative shift is expected; the test asserts on *behavioral* boundaries — e.g. "100 % solid content stays in tile-codec mode" — not on exact thresholds.)

### Testing

- The bench results file itself is the test artifact — reviewed manually before M3.4 unblocks.
- The classifier-constants commit is reviewed against the bench markdown for consistency.

### Out of scope for M3.5

- Adaptive online cost-model profiling (recorded as future work).
- Cross-hardware bench validation (results valid for the hardware the bench ran on; cross-hardware portability is future work).

---

## Wire Protocol Additions (consolidated)

### Existing M2 discriminators (unchanged)

| Type | Direction | Purpose |
|---|---|---|
| `0x01` (FEEDBACK) | client → server, reliable bidi | `ReceiverFeedback`, every 100 ms |
| `TILE_DATAGRAM_FLAG` (high bit on `frame_seq`) | server → client, datagram | Tile-codec emission |
| (no flag) | server → client, datagram | Full-frame H.264 |

### M3 additions

| Type | Direction | Phase | Reliability | Purpose |
|---|---|---|---|---|
| `ACK_BATCH_MSG_TYPE = 0x02` | client → server | M3.1 | datagram (unreliable) | Batched tile/pass ACKs |

That's the only new top-level discriminator. Palette delivery rides inline in PalRle tile datagrams (codec-specific framing — see M3.2).

### Generation + pass packing (lands in M3.1)

`TileHeader` byte `[3]` becomes `(generation << 4) | pass_idx` — 4 bits each. Header stays at 8 bytes total. M3.0 does not need this packing (no scheduler, no passes); M3.1 ships it alongside the scheduler and ACK protocol.

```
[3]    (gen: u4) << 4 | (pass: u4)
```

- **Gen wrap (16 generations):** acceptable because the scheduler aggressively cancels in-flight stale entries on every `bump_generation`. No old-gen packet is in flight when wrap-around reuses a value. Retry interval is 2 × RTT (~20–100 ms) — in-flight clearance is fast relative to wrap risk at 60 fps.
- **Pass cap (16 passes):** CDF 5/3 caps at 9 per source spec; comfortable headroom.

M2 sites currently set `generation = 0`; they continue to work as `gen=0, pass=0`. No breaking change vs M2.

### Frame-mode signaling

No new discriminator. The existing distinction (frame-seq high bit set → tile datagram; clear → full-frame H.264 datagram) already encodes mode at the per-datagram level. The M3.0 mode-switch rule chooses which kind of datagram to emit per `frame_seq`; the client routes to the right decoder by inspecting the bit.

### Codec enum

Already has all six variants (`Skip = 0, H264 = 1, PalRle = 2, Bc1 = 3, Solid = 4, Raw = 5, Cdf53 = 6`) since M2 — no change.

### Reliable bidi stream direction

Stays client→server only, used solely for `ReceiverFeedback`. No server→client reliable channel is needed for M3.

### Migration safety

All wire additions are net-new message types with their own discriminators. No existing M2 message format changes. Existing decoders that currently set `generation = 0` continue to work.

---

## Cross-Cutting Concerns

### Testing strategy summary

| Test layer | Coverage |
|---|---|
| Unit | Classifier rule table, cost-model boundary cases, `Scheduler` round-robin/retry/cancellation, `AckBatcher`, `PaletteTable` lifecycle, codec encode/decode roundtrips |
| Property | Mode-switch hysteresis monotonicity, codec encode → decode bit-exactness for lossless codecs, refinement SSIM monotonicity, M3 classifier invariants C1–C6 currently TODO at `tile/tests_proptest.rs:289` |
| E2E | One new test per phase: `e2e_mode_switch`, `e2e_solid_color` extension, `e2e_ack_loss`, `e2e_text_clarity`, `e2e_palette_eviction`, `e2e_progressive_refinement`, `e2e_refinement_cancel`, optionally `e2e_bc1_gradient` |
| Bench | Existing harness; M3.5 fills in `BenchEncoder` impls and publishes results |

### Future work (recorded so it isn't lost)

1. **Adaptive online cost-model profiling.** Replaces hardcoded `CostModel` constants with measurements from observed encode times via online EMA tracking. Deferred because hardcoded constants are sufficient for first deployment and adaptive profiling carries observability/oscillation risk.
2. **Cross-hardware bench portability.** M3.5 publishes results from the hardware it ran on. Different GPUs may have different cost ratios; future work to either run benches on a hardware matrix or ship a fast-path startup micro-benchmark adapting constants per machine (overlaps with #1).
3. **Palette delivery via dedicated control channel** if piggybacking ever proves insufficient (e.g. pathological palette churn relative to tile churn). Today's design intentionally avoids this complexity.
4. **Intra-refresh tuning across H.264 mode re-entries.** Current plan forces an I-frame on every re-entry. If mode switches turn out to be frequent, this might be too costly; future work to evaluate alternatives.
5. **More than 16 generations / passes per tile** — if a future codec needs > 16 passes, the gen+pass byte split is the constraint to revisit.
6. **Trend-tracking in the mode-switch decision.** Augment cost-only entry/exit with a short rolling window of past frame costs and per-frame H.264-tile fractions, so the classifier predicts video-ish streams before the steady-state cost rule catches up. Replaces what the source spec §4.3 metric-based rules were originally hand-tuned to do; intentionally deferred from M3.0 because the cost rule is sufficient for first deployment and trend logic carries oscillation risk that benefits from real bench data (M3.5).
7. **CPU-path full-frame H.264 emission.** M3.0 keeps the CPU path (`process_frame_cpu`) on tile-codec-Raw because no GPU is the degraded fallback case. If CPU-path quality becomes a real concern, wire `FullFrameEncoder` (libx264 backend) into the CPU path so it can also emit H.264 mode.

### Explicit out-of-scope for M3

Preserving source-spec §13 non-goals where relevant:
- Multi-monitor / display-config (M4).
- Audio (M5).
- NVENC (M6).
- Native client (post-M3).
- Software-only codec fallbacks.
- Operation without Tailscale.

---

## Decision Register

| # | Decision | Rationale |
|---|---|---|
| D1 | Frame-mode switch (no mixing of H.264 + per-tile in same `frame_seq`) | Avoids encoder-fights-encoder problem; matches user direction. |
| D2 | Six-phase split; each phase shippable with E2E gate | Each cycle is single-session-sized; visible value at every step. |
| D3 | Cost-aware mode switch with hardcoded constants in M3.0 | Simplest path to a working classifier; M3.5 closes the loop with real data. |
| D4 | Adaptive online cost profiling deferred to future work | Adds complexity without proven need; hardcoded + retune is sufficient. |
| D5 | Codec-agnostic scheduler; Solid is the vehicle for the infrastructure | Solid exercises every part of the plumbing except multi-pass advancement. |
| D6 | Batched datagram ACKs (option B) — not reliable stream, not piggyback in `ReceiverFeedback` | Reliable adds latency; `ReceiverFeedback`'s 100 ms cadence is too slow for ACK-driven retry; lost ACKs are harmless. |
| D7 | Generation + pass packed as 4 + 4 bits in existing `TileHeader` byte | Saves wire bytes; aggressive supersession makes 16-generation wrap safe. |
| D8 | Palette delivery via inline piggyback on tile datagrams; no separate `PaletteUpdate` message | Self-healing: any ACKed PalRle tile delivers its palette; retry of tile naturally retries palette. |
| D9 | `delivered[id]` cleared on slot reallocation with new content (not on `ref_count → 0`) | Avoids unnecessary palette retransmits when content briefly flips away and back. |
| D10 | M3.4 (BC1) is conditional on M3.5 bench dominance check | Source spec §12 anticipates BC1 may be dropped if CDF 5/3 dominates. |
| D11 | Per-codec brainstorm + design spec required before writing-plans for M3.2, M3.3, M3.4 | Codec-specific implementation details are too deep to lock in the umbrella. |
| D12 | Remove per-tile `H264VaapiEncoder` in M3.0 | No longer used under the frame-mode architecture. |
| D13 | M3.0 CPU path stays in tile-codec-Raw mode; no `FullFrameEncoder` wire-up CPU-side | CPU path is the degraded fallback when no GPU; emitting Raw matches that reality. Tracked in Future Work #7. |
| D14 | M3.0 replaces source-spec §4.3 metric-based H.264 enter/exit rules with cost-based rules + sustained-motion fast-path | Cost generalizes as new codecs land and as `bytes_per_us` becomes link-aware; trend-tracking that re-introduces frequency awareness without hand-tuned thresholds is post-M3 (Future Work #6). |
| D15 | M3.0 `MOTION_TILE_MIN_ABSOLUTE` floor on the fast-path | Fraction-only rule trips on cursor blinks and single-pixel UI ticks; an absolute floor prevents whole-frame H.264 promotion for trivial dirty counts. |
| D16 | M3.0 GPU-derived metrics use sentinel values; rules consulting them treat sentinel as "no match" | GPU compute backing for `unique_colors` / `edge_density` doesn't land until M3.3; sentinels keep the type populated and tests inject concrete values. |
| D17 | `bytes_per_us` placeholder in M3.0; wired to the §6.5 estimator once it exists (M4+) | The estimator is upstream future work; classifier reads through one accessor so swap-in is a one-call change. |
| D18 | Force-IDR via new `FullFrameEncoder.request_keyframe()` one-shot flag | Existing API computes `force_idr` only from PTS; mode re-entry needs an external trigger. |

---

## Document Pointers

- Source spec sections referenced throughout: §4 Tile Engine, §5 Encoding Pipeline, §12 Milestones in `docs/specs/ghostframe-initial-spec.md`.
- Predecessor designs: `docs/superpowers/specs/2026-04-11-implementation-plan-m0-m3-design.md`, `docs/superpowers/specs/2026-04-23-m2-zero-copy-gpu-pipeline-design.md`.
- Bench harness: `docs/superpowers/plans/2026-04-27-codec-bench-skeleton.md`, `ghostframe-lib/benches/`.
- Per-codec brainstorm artifacts (to be authored before M3.2/M3.3/M3.4 writing-plans): `docs/superpowers/specs/YYYY-MM-DD-{palrle,cdf53,bc1}-codec-design.md`.
