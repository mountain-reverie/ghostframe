# M3.2 — PalRLE Codec + Palette Table: Per-Codec Design

**Date:** 2026-05-13
**Status:** Design approved
**Predecessors:**
- `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` (M3 umbrella; §M3.2 envelope)
- `docs/superpowers/specs/2026-05-11-m3.1-solid-scheduler-decisions.md` (M3.1 Solid + scheduler + ACK infrastructure)
- `docs/superpowers/specs/2026-05-11-gpu-tile-analysis-design.md` (GPU `tile_analysis.comp` producing `count`, `edge_density`, `colors[16]`)

This design supersedes the M3 umbrella's §M3.2 envelope where they conflict — specifically: pipeline shape, classifier interface, palette-table semantics, and client implementation choice. The umbrella stays the source of truth for the wider phase ordering (M3.3+) and for items unchanged here.

---

## Context

After M3.1 merged, the path to PalRLE has fewer obstacles than the umbrella anticipated:

1. **GPU `tile_analysis.comp` already exists** and produces canonical-sortable per-tile palette data — what the umbrella parked behind "M3.3 GPU compute" shipped pre-M3.2.
2. **Classifier rules 3 and 7 already select `PalRle { palette_id: 0 }`** on real GPU-derived `unique_colors`. The placeholder palette id is the only thing standing between the classifier and end-to-end PalRLE emission.
3. **Scheduler, ACK protocol, retry loop, generation invalidation, and `dispatch_dirty_tiles_via_scheduler`** are all in place from M3.1. PalRLE rides existing transport infrastructure unchanged.

What's missing is the encoder, the palette table, the wire-format codec branch, and the client decode path. The umbrella spec sketched these but left key questions open — most notably how palette delivery interacts with retries, how the GPU-derived palette data flows into the encoder, and whether the encode work itself moves to GPU.

This spec answers those questions and splits the milestone into two sessions:

- **M3.2a** *(this design — current session)*: server-side GPU encode pipeline, CPU client decode, persistent palette table with delivery tracking. Ships fully working PalRLE end-to-end with Canvas 2D client rendering.
- **M3.2b** *(deferred — next session)*: client renderer migration to WebGPU. Touches every codec path, not just PalRLE. Server-side work from M3.2a survives unchanged.

The umbrella's "WASM module" deliverable is likely skipped entirely — WebGPU in M3.2b supersedes the use case.

---

## Decision Register

| # | Decision | Rationale |
|---|---|---|
| D1 | Split M3.2 into M3.2a (server GPU encode + CPU client) and M3.2b (client WebGPU migration) | The umbrella's "WASM client" intermediate is wasted infrastructure if WebGPU is the next step; the client surface change deserves its own session |
| D2 | M3.2a takes **Approach B** — GPU does palette fold + index emission, CPU does final nibble-RLE byte-pack via rayon | Preserves the "no-readback of BGRA pixels" strategic property; CPU RLE on 4-bit indices is cheap; wire format stays exactly the umbrella's |
| D3 | Single-client invariant: at most one WebTransport session connected at a time; new connection displaces existing | Simplifies `PaletteTable` to flat server-wide state with no per-session iteration. Multi-monitor (M4) will fit additional displays into the same connection via protocol extensions |
| D4 | Slot byte-overwrite requires `delivered[id] == true` OR `in_flight_carrying[id] == 0` | Closes the umbrella's collision bug — reallocating a slot whose previous bytes are in flight would corrupt rendering for the tile carrying the stale id |
| D5 | Subset-folding (Stage 2b) lands in M3.2a, not deferred | Canonical-sort property makes the implementation elegant: Stage 3's per-pixel binary search against the folded-into palette produces correct indices without any explicit remap table |
| D6 | Stage 1.5 compaction pass produces a dense PalRLE-feasible-dirty tile list; Stages 2a/2b/3 indirect-dispatch over it | Cleaner CPU iteration in Stage 4 (no sparse gather), simpler shader entry points, naturally bounds Stage 2a's hash-table sizing |
| D7 | Classifier stays pure — `classify_tile(metrics, prev_state)` signature unchanged | Threading palette-table view into the classifier doubles bookkeeping; allocation is downstream of classification regardless |
| D8 | `palette_id == 0` overloaded as "feasibility placeholder" pre-Phase-A, "real slot 0" post-Phase-A | Avoids an enum change rippling through proptest invariants; the pre/post-Phase-A boundary is clear in code |
| D9 | rayon at the tile level for Stage 4 encode; SIMD inner-loop deferred to M3.5-data-driven follow-up | Per-tile encode is embarrassingly parallel and intrinsically cheap; SIMD would be optimization without a profile |
| D10 | Pure TypeScript client decode in M3.2a; WASM not built | M3.2b's WebGPU replaces this code; WASM is wasted intermediate infrastructure |
| D11 | Allocation failure → per-tile `Codec::Raw` fallback, `codec_state` set to `Skip` for the frame | Bounded degradation; classifier re-evaluates next frame; affected tiles still render correctly with larger payload |
| D12 | `Scheduler::on_ack` callback grows a return value so IoBridge can update palette-table delivery state | Single seam between transport and palette layers; scheduler stays codec-agnostic |
| D13 | `index_buffer` compact-indexed via Stage 1.5 (one 512 B slot per dirty-PalRLE tile, not per total tile) | Cleaner iteration shape in Stage 4 even though worst-case allocation is unchanged |
| D14 | `atomicOr` on shared `uint` for Stage 3 output packing — Vulkan 1.0 core, no extension or feature query needed | Verified against Vulkan-Guide and limits spec; AMD GCN+ and Intel Gen9+ baseline-supported |

---

## Architecture

### Pipeline shape

M3.2a adds four new GPU compute shaders (plus an extension to the existing `tile_analysis.comp`) and one CPU encode stage for tiles classified as PalRLE-feasible:

```
                       [per-frame, dirty PalRLE-feasible tiles only]
                                          │
   Stage 1 (existing, extended)  ─► tile_analysis.comp
                                    + 16-element bitonic sort of colors[]
                                    outputs: count, edge_density, colors[16] sorted
                                          │
   Stage 1.5 (new)               ─► palrle_compact.comp
                                    reads SAD + tile_analysis.count
                                    outputs: palrle_compact_list[],
                                             palrle_compact_count,
                                             indirect-dispatch args
                                          │
   Stage 2a (new)                ─► palette_fold.comp
                                    indirect-dispatched over compact list
                                    outputs: frame_palette_set[≤256],
                                             frame_palette_set_count,
                                             per_tile_frame_palette_id[]
                                          │
   Stage 2b (new)                ─► palette_subset_fold.comp
                                    outputs: folded_into[256]
                                          │
   Stage 3 (new)                 ─► pal_rle_index.comp
                                    per-pixel binary search vs folded palette
                                    outputs: index_buffer[compact_count × 512 B]
                                          │
                                  ━━━ GPU/CPU boundary (single fence) ━━━
                                          │
   Stage 4 (CPU)                 ─► IoBridge:
                                    Phase A (serial):
                                       classifier decisions → palette-table
                                       find_matching/allocate → preps
                                    Phase B (rayon parallel):
                                       per-tile encode_pal_rle_payload
                                    Phase C (serial):
                                       integrate into dispatch_dirty_tiles_via_scheduler
                                       scheduler.enqueue with codec=PalRle
                                          │
                                    fragment_tile → datagrams → wire
```

All GPU stages record into the same command buffer that already carries SAD + NV12 conversion. One fence-wait at end-of-frame. No GPU↔CPU sync mid-pipeline.

### Component ownership

| Component | Owner | Module |
|---|---|---|
| GPU pipeline orchestration | `GpuFrameProcessor` | `capture/gpu_pipeline.rs` |
| New compute shaders | embedded SPIR-V | `capture/shaders/palrle_compact.comp`, `palette_fold.comp`, `palette_subset_fold.comp`, `pal_rle_index.comp` |
| `PaletteEntry`, `SlotState`, `PaletteTable`, `FramePaletteStats` | `IoBridge` | `encoder/pal_rle.rs` |
| `encode_pal_rle_payload`, `encode_pal_rle_indices` | free functions | `encoder/pal_rle.rs` |
| Classifier (unchanged) | `Classifier` | `tile/classifier.rs` |
| Phase A/B/C dispatch | `IoBridge::dispatch_dirty_tiles_via_scheduler` extension | `transport/io_bridge.rs` |
| Client `PaletteCache` | new module | `ghostframe-web-client/src/palette.ts` |
| Client `decodePalRle`, `drawPalRleTile` | extended dispatch | `ghostframe-web-client/src/decoder.ts`, `renderer.ts` |

### Buffer residency audit

#### Truly GPU-only (never reach CPU)

| Buffer | Use |
|---|---|
| BGRA capture image | Stage 3 sampler |
| NV12 image | unrelated (H.264 path) |
| Stage 2a frame hash table | atomic probing scratch |
| Stage 1 bitonic-sort scratch (shared memory) | sort comparators |
| Stage 3 `uint scratch[256]` (shared memory, 1024 nibbles) | per-workgroup index pack |
| Stage 3 cooperative palette load (shared memory, 16 entries) | per-pixel lookup |
| `palrle_indirect_dispatch_args` | `vkCmdDispatchIndirect` input |

#### Mandatorily CPU-readable (HOST_VISIBLE | HOST_COHERENT)

| Buffer | Produced by | Read by CPU for | Size (1080p worst) |
|---|---|---|---|
| `tile_analysis` (`count`, `edge_density_thou`) | Stage 1 | `populate_gpu_metrics` → classifier inputs | 160 KB (existing buffer) |
| `palrle_compact_list` | Stage 1.5 | tile-index lookup in Stage 4 | N_tiles × 4 B = 8 KB |
| `palrle_compact_count` | Stage 1.5 | iteration bound | 4 B |
| `frame_palette_set` | Stage 2a | `PaletteTable::find_matching` + bundled wire bytes | 256 × 64 B = 16 KB |
| `frame_palette_set_count` | Stage 2a | iteration bound | 4 B |
| `per_tile_frame_palette_id` | Stage 2a | maps compact slot → frame palette id | compact_count × 1 B ≤ 2 KB |
| `folded_into` | Stage 2b | resolves frame palette id → effective palette id | 256 B |
| `index_buffer` | Stage 3 | rayon RLE-pack input | compact_count × 512 B ≤ 1 MB |

Typical-case CPU read surface: ~100–200 KB per frame (text-heavy stream with ~100–300 PalRLE tiles).

---

## Server-side GPU encode pipeline

### Stage 1 — `tile_analysis.comp` extension (canonical sort)

The existing shader's bucket-flatten phase produces `colors[count]` in arbitrary slot order. Append a **16-element bitonic sort by BGRA-as-`u32` ascending** using the first 16 threads of the workgroup:

- 4 stages, log₂16 = 4 levels of compare-exchange.
- Required by Stage 2a's hash-on-sorted-bytes dedup and `PaletteTable::find_matching`'s set-equality-via-memcmp check.
- Bytes beyond `count` remain zero (existing contract).

### Stage 1.5 — `palrle_compact.comp` (new)

**Purpose:** produce the dense list of tile indices that satisfy "dirty AND `count ≤ 16`". All later stages indirect-dispatch over this list.

**Dispatch:** one workgroup per tile (cheap; mostly early-exits).

**Per-workgroup work (single thread `gl_LocalInvocationID == 0`):**

```glsl
uint tile_idx = gl_WorkGroupID.x;
if (sad[tile_idx] >= DIRTY_THRESHOLD && tile_analysis[tile_idx].count <= 16) {
    uint slot = atomicAdd(palrle_compact_count, 1u);
    palrle_compact_list[slot] = tile_idx;
}
```

After all workgroups complete, a separate "indirect-args writer" pass (one workgroup, one thread) reads `palrle_compact_count` and writes the indirect-dispatch arguments:

```glsl
dispatch_args.x = palrle_compact_count;
dispatch_args.y = 1u;
dispatch_args.z = 1u;
```

Two-pass form avoids the "last workgroup writes the args" race detection complexity. Pipeline barrier between the two: `SHADER_WRITE → SHADER_READ`.

**Buffers introduced:**

| Buffer | Residency | Size |
|---|---|---|
| `palrle_compact_list` | host-visible | N_tiles_max × 4 B |
| `palrle_compact_count` | host-visible | 4 B |
| `palrle_indirect_dispatch_args` | DEVICE_LOCAL | 12 B (`uvec3`) |

### Stage 2a — `palette_fold.comp` (new, dedup)

**Purpose:** within the frame, dedup identical palettes so multiple tiles share one `frame_palette_set` entry.

**Dispatch:** `vkCmdDispatchIndirect(palrle_indirect_dispatch_args)`. One workgroup per compact-slot index `c`; resolves tile via `tile_idx = palrle_compact_list[c]`. 16 threads per workgroup.

**Per-workgroup work:**

1. Thread 0 computes FNV-1a hash of the canonical-sorted `tile_analysis[tile_idx].colors[0..count]`.
2. `atomicCAS`-probe a frame-scoped open-addressed hash table at `hash % 256`, linear probe on collision (probe budget = 16).
3. On insert: write palette bytes into `frame_palette_set[slot]`, set `unique_count` via `atomicMax`.
4. On hit: byte-compare against the slot's existing bytes (16-color hash collisions are real); if equal, reuse; else continue probing.
5. After probe-budget exhaustion: write `per_tile_frame_palette_id[c] = 0xFF` (sentinel — Phase A treats as table-full).
6. On any hit/insert: write `per_tile_frame_palette_id[c] = slot`.

**Outputs:**

| Buffer | Size |
|---|---|
| `frame_palette_set: [PaletteEntry; 256]` (canonical-sorted bytes per unique frame palette) | 16 KB |
| `frame_palette_set_count: u32` | 4 B |
| `per_tile_frame_palette_id: [u8; compact_count_max]` | ≤ 2 KB |

### Stage 2b — `palette_subset_fold.comp` (new, subset fold)

**Purpose:** detect when one frame palette A is a subset of another frame palette B and arrange for A's tiles to use B instead, so the wire carries one bundled palette (B) instead of two.

**Dispatch:** one workgroup per unique frame palette (indirect from `frame_palette_set_count`). 256 threads per workgroup.

**Per-thread work:**

- Each thread `t` examines `B = frame_palette_set[t]` as candidate superset for the workgroup's `A = frame_palette_set[wg_id]`.
- Early-out: `t == wg_id`, or `count_B < count_A`, or `t >= frame_palette_set_count`.
- Linear sorted-set inclusion: walk A and B cooperatively in sorted order; A ⊆ B iff every element of A matches some element of B in increasing-index order. O(count_A + count_B) ≤ 32 compares.
- If A ⊆ B, encode tiebreaker key and `atomicMin` against `folded_into[wg_id]`:

  ```
  key = ((255 - count_B) << 8) | t   // larger count wins; ties broken by lower frame_palette_id
  atomicMin(folded_into[wg_id], key)
  ```

- Workgroup barrier; thread 0 rewrites `folded_into[wg_id]` to the low byte of the winning key.

**Initialization:** `folded_into[i] = (0u << 8) | i` for each i (default-self with the highest tiebreaker, so any real superset wins).

**Outputs:**

| Buffer | Size |
|---|---|
| `folded_into: [u8; 256]` | 256 B |

**Why canonical sort makes this elegant:** because every palette is BGRA-ascending-sorted, the index of a color in A and the index of the same color in B differ only structurally — but Stage 3 doesn't care about A's indices, it just searches B directly. No color-remap table needed.

### Stage 3 — `pal_rle_index.comp` (new, index emission)

**Purpose:** per-pixel palette lookup → tightly-packed 4-bit index stream.

**Dispatch:** `vkCmdDispatchIndirect(palrle_indirect_dispatch_args)`. One workgroup per compact slot; 32×32 = 1024 threads per workgroup.

**Per-thread work:**

```glsl
uint c = gl_WorkGroupID.x;
uint tile_idx = palrle_compact_list[c];
uint effective_pal_id = uint(folded_into[per_tile_frame_palette_id[c]]);
PaletteEntry palette = frame_palette_set[effective_pal_id];  // cooperative shared load — see below

ivec2 pixel = ivec2(gl_LocalInvocationID.xy);
uint tile_x = tile_idx % cols;
uint tile_y = tile_idx / cols;
ivec2 frame_pixel = ivec2(tile_x * 32u + pixel.x, tile_y * 32u + pixel.y);
vec4 bgra = imageLoad(current_frame, frame_pixel);
uint packed = pack_bgra(bgra);

// Binary search on canonical-sorted palette (log₂16 = 4 compares).
uint idx = binary_search(palette.colors, palette.count, packed);

// Atomic-OR into shared scratch.
uint pixel_idx = uint(pixel.y) * 32u + uint(pixel.x);
uint word = pixel_idx / 8u;     // 0..127
uint shift = (pixel_idx % 8u) * 4u;
atomicOr(scratch[word], idx << shift);
```

After workgroup barrier, 128 threads (subset 0..127) write 128 `uint`s = 512 bytes to `index_buffer[c * 512 ..]`.

**Cooperative palette load:** before the binary-search work, 16 threads with `gl_LocalInvocationID < 16` cooperatively load `palette.colors[0..16]` from `frame_palette_set` into workgroup-shared memory. Workgroup barrier. Subsequent binary searches read from shared memory at ~10× lower latency than the global SSBO.

**Outputs:**

| Buffer | Size (worst case 1080p, all tiles PalRLE-feasible) |
|---|---|
| `index_buffer: [u8; compact_count × 512]` | ~1 MB |

Typical-case touched bytes: dirty-PalRLE-tile count × 512 B ≈ 50–150 KB.

### Pipeline barriers + submit shape

Stages chain via Vulkan pipeline barriers, all `VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT → VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT` with the appropriate `SHADER_WRITE → SHADER_READ` / `INDIRECT_COMMAND_READ` access masks:

```
[capture]
  → tile_sad.comp
  → tile_analysis.comp (with sort)
[barrier: tile_analysis SHADER_WRITE → SHADER_READ]
  → palrle_compact.comp (writes list, count)
  → palrle_indirect_args_writer.comp (1 dispatch, 1 thread)
[barrier: indirect args SHADER_WRITE → INDIRECT_COMMAND_READ]
  → palette_fold.comp (indirect dispatch)
[barrier: frame_palette_set SHADER_WRITE → SHADER_READ]
  → palette_subset_fold.comp (per-unique-palette dispatch)
[barrier: folded_into SHADER_WRITE → SHADER_READ]
  → pal_rle_index.comp (indirect dispatch)
[fence wait]
[CPU reads]
```

All recorded into the existing command buffer. One submit, one fence-wait at end of frame.

---

## Stage 4 — CPU pipeline

### Phase A — serial palette-table mapping

Runs after classifier evaluation (so `metrics_tracker.codec_state` reflects the final classifier decision per tile). Iterates the GPU's compact list:

```rust
struct PalRleTileWorkPrep<'a> {
    tile_idx_in_grid: (u32, u32),
    indices_slice:    &'a [u8],         // 512 B from index_buffer compact slot
    palette_bytes:    &'a PaletteEntry, // ref into frame_palette_set (effective, post-fold)
    persistent_palette_id: Option<u8>,
    bundled: bool,
}

let mut preps: Vec<PalRleTileWorkPrep> = Vec::with_capacity(palrle_compact_count);
for c in 0..palrle_compact_count {
    let tile_idx = palrle_compact_list[c];
    let (tx, ty) = (tile_idx % cols, tile_idx / cols);

    // Honour classifier's actual decision — tile may have been PalRLE-feasible
    // (count ≤ 16) but classifier picked H264/BC1 via freq/magnitude rules.
    if !matches!(metrics_tracker.get(tx, ty).codec_state,
                 CodecState::PalRle { .. }) {
        continue;
    }

    let frame_pal_id_local = per_tile_frame_palette_id[c];
    if frame_pal_id_local == 0xFF {
        // Stage 2a sentinel — frame-palette-set overflow.
        metrics_tracker.get_mut(tx, ty).codec_state = CodecState::Skip;
        palette_table.stats_frame.fell_back_to_raw += 1;
        continue;
    }

    let frame_pal_id = folded_into[frame_pal_id_local as usize] as usize;
    let palette = &frame_palette_set[frame_pal_id];

    let (persistent_id, bundled) = match palette_table.acquire_or_allocate(palette) {
        Some(id) => {
            let bundled = !palette_table.delivered.contains(id);
            if bundled {
                palette_table.in_flight_carrying[id as usize] += 1;
            }
            (Some(id), bundled)
        }
        None => (None, false),  // table full
    };

    if let Some(id) = persistent_id {
        metrics_tracker.get_mut(tx, ty).codec_state = CodecState::PalRle { palette_id: id };
        palette_table.stats_frame.reused_or_allocated += 1;
        preps.push(PalRleTileWorkPrep {
            tile_idx_in_grid: (tx, ty),
            indices_slice:    &index_buffer[c * 512 .. (c + 1) * 512],
            palette_bytes:    palette,
            persistent_palette_id: Some(id),
            bundled,
        });
    } else {
        metrics_tracker.get_mut(tx, ty).codec_state = CodecState::Skip;
        palette_table.stats_frame.fell_back_to_raw += 1;
    }
}
```

The `acquire_or_allocate` helper encapsulates the 4-way ladder:

```rust
fn acquire_or_allocate(&mut self, palette: &PaletteEntry) -> Option<u8> {
    // 1. find_matching over (Held ∪ FreeButCached)
    if let Some(id) = self.find_matching(palette) {
        self.acquire(id);
        return Some(id);
    }
    // 2. find_eligible_free_slot (FreeButCached with overwrite_eligible)
    if let Some(id) = self.find_eligible_free_slot() {
        self.write_bytes(id, palette);
        self.delivered.remove(id);
        self.in_flight_carrying[id as usize] = 0;
        self.acquire(id);
        return Some(id);
    }
    // 3. find any Empty slot
    if let Some(id) = self.find_empty_slot() {
        self.write_bytes(id, palette);
        self.acquire(id);
        return Some(id);
    }
    // 4. fail
    None
}
```

### Phase B — rayon parallel encode

```rust
let encoded: Vec<((u32, u32), Vec<u8>)> = preps
    .par_iter()
    .map(|p| {
        let pid = p.persistent_palette_id.expect("Phase A filtered Nones");
        let payload = encode_pal_rle_payload(
            p.indices_slice,
            p.palette_bytes,
            pid,
            p.bundled,
        );
        (p.tile_idx_in_grid, payload)
    })
    .collect();
```

`encode_pal_rle_payload` is a pure function — see "Wire format" section below.

**Rayon warm-up:** `IoBridge::new` runs `(0..1).into_par_iter().for_each(|_|{})` at construction time to amortize thread-pool spin-up cost off the first-frame hot path.

### Phase C — integrate into dispatch loop

`dispatch_dirty_tiles_via_scheduler` already iterates all dirty tiles. M3.2a hands it the precomputed PalRLE payloads:

```rust
let mut palrle_by_tile: HashMap<(u32, u32), Vec<u8>> = encoded.into_iter().collect();

for &(tile_x, tile_y) in dirty {
    self.scheduler.bump_generation(tile_x as u8, tile_y as u8);
    let gen = self.scheduler.generation_for(tile_x as u8, tile_y as u8);

    let (codec, payload) = match policy {
        SchedulerEmissionPolicy::CpuRawOnly => {
            (Codec::Raw, grid.extract_tile(pixels, stride, tile_x, tile_y))
        }
        SchedulerEmissionPolicy::GpuClassifierDriven => {
            match self.metrics_tracker.get(tile_x, tile_y).codec_state {
                CodecState::Solid => {
                    let t = grid.extract_tile(pixels, stride, tile_x, tile_y);
                    (Codec::Solid, encode_solid(&t).to_vec())
                }
                CodecState::PalRle { .. } => match palrle_by_tile.remove(&(tile_x, tile_y)) {
                    Some(p) => (Codec::PalRle, p),
                    None    => (Codec::Raw, grid.extract_tile(pixels, stride, tile_x, tile_y)),
                },
                _ => (Codec::Raw, grid.extract_tile(pixels, stride, tile_x, tile_y)),
            }
        }
    };

    self.scheduler.enqueue(TileWork { /* ... */ codec, payload, /* ... */ });
}
```

### ACK / supersession callback

`Scheduler::on_ack` gains a return value (or accepts a `&mut dyn FnMut(&TileWork)` callback) so the IoBridge can update the palette table when a PalRle tile is acknowledged:

```rust
// IoBridge::dispatch_ack_datagram
let resolved: Vec<TileWork> = self.scheduler.on_ack_collecting(tile_x, tile_y, gen, pass);
for w in resolved {
    if w.codec == Codec::PalRle {
        let palette_id = extract_palette_id_from_payload(&w.payload);  // byte [1]
        if !self.palette_table.delivered.contains(palette_id) {
            self.palette_table.delivered.insert(palette_id);
        }
        self.palette_table.in_flight_carrying[palette_id as usize] =
            self.palette_table.in_flight_carrying[palette_id as usize].saturating_sub(1);
    }
}
```

Symmetric path for scheduler-internal supersession (a generation bump cancels in-flight work): the resolved `TileWork` flows through the same callback, decrementing `in_flight_carrying` without flipping `delivered`.

### Frame stats finalization

After Phase C:

```rust
tracing::debug!(
    target = "palrle.frame",
    reused_or_allocated   = palette_table.stats_frame.reused_or_allocated,
    fell_back_to_raw      = palette_table.stats_frame.fell_back_to_raw,
    unique_frame_palettes = frame_palette_set_count,
    "palrle frame stats"
);
palette_table.stats_frame = FramePaletteStats::default();
```

---

## Persistent `PaletteTable`

### Structure

```rust
pub struct PaletteEntry {
    pub colors: [[u8; 4]; 16],   // BGRA, canonical-sorted (BGRA ascending as u32)
    pub count:  u8,               // 1..=16
}

pub enum SlotState { Empty, Held, FreeButCached }

pub struct PaletteTable {
    entries:            [Option<PaletteEntry>; 256],
    slot_state:         [SlotState; 256],
    ref_count:          [u32; 256],
    delivered:          BitSet<256>,
    in_flight_carrying: [u32; 256],
    free_lru:           VecDeque<u8>,
    stats_frame:        FramePaletteStats,
}

#[derive(Default)]
pub struct FramePaletteStats {
    pub reused_or_allocated: u32,
    pub fell_back_to_raw:    u32,
}
```

`PaletteTable` lives directly in `IoBridge`. No per-session wrapping (single-client invariant — D3).

### Slot lifecycle

```
                    write_bytes (new palette content)
                              │
   Empty  ────────────────────►  Held { delivered: false, in_flight: 0 }
                              │
                              │   acquire (ref_count++)
                              │
   Held { delivered: false }  ───first-tile-ACKed──►  Held { delivered: true }
                              │                  │
                              │   release → 0    │   release → 0
                              ▼                  ▼
   FreeButCached { delivered: false }   FreeButCached { delivered: true }
        │                                  │
   overwrite_eligible iff               always overwrite_eligible
   in_flight_carrying == 0
```

### Repurpose-eligibility predicate (single-client form)

```rust
fn overwrite_eligible(&self, id: u8) -> bool {
    self.ref_count[id as usize] == 0
        && (self.delivered.contains(id)
            || self.in_flight_carrying[id as usize] == 0)
}
```

Reasoning: a slot's bytes can be safely overwritten when no in-flight tile could observe a mismatch between the bytes the server thought it sent and the bytes the slot now contains. Two sufficient conditions:

- **Bytes already received** (`delivered == true`): the client has rendered using these exact bytes; the in-flight tile (if any) references valid bytes.
- **Bytes never sent** (`in_flight_carrying == 0 AND delivered == false`): the slot's current byte version has never reached the wire, so no in-flight tile references them.

### Per-frame allocation algorithm

See `acquire_or_allocate` in Phase A above. The 4-way ladder:

1. `find_matching` — full byte-equal scan over (Held ∪ FreeButCached). Returns existing slot id; `acquire` increments ref_count.
2. `find_empty_slot` — first `Empty`. On hit: write bytes, acquire.
3. `find_eligible_free_slot` — `free_lru` oldest entry that passes `overwrite_eligible`. On hit: overwrite bytes, clear delivered, zero `in_flight_carrying`, acquire.
4. fail (return None) — caller falls back to `Codec::Raw` for the tile.

**Rationale for the path-2/path-3 order**: prefer truly-fresh empty slots to extend cache lifetime; only evict a FreeButCached entry when the cache is genuinely full. An earlier version of this design (pre-2026-05-18) had paths 2 and 3 swapped, which thrashed slot 0 on small palette working sets — a 2-color flip would call `find_eligible_free_slot` and get the same oldest slot every cycle, then `write_bytes` would clear `delivered`. The existing FreeButCached entry for the OTHER colour was never used. With the current order, an N=2 working set occupies slots 0 and 1 and `find_matching` hits both stably; eviction only kicks in once all 256 slots are non-Empty.

### Connection lifecycle

On new WebTransport session arrival, the IoBridge:

1. `scheduler.clear()` (existing M3.1 behavior).
2. `palette_table.on_session_reset()`:

   ```rust
   fn on_session_reset(&mut self) {
       self.delivered.clear();
       self.in_flight_carrying.fill(0);
       self.ref_count.fill(0);
       // Slots with bytes → FreeButCached; Empty slots stay Empty.
       // Bytes are NOT cleared — find_matching may still hit next frame.
       for (id, entry) in self.entries.iter().enumerate() {
           self.slot_state[id] = if entry.is_some() { SlotState::FreeButCached }
                                 else              { SlotState::Empty };
       }
       self.rebuild_free_lru();
   }
   ```

Warm-cache rationale: text colors are wildly repeated across sessions to the same content (D3 follow-up confirmation); preserving slot bytes lets the next session's first frame hit `find_matching` and bundle on first use rather than re-deriving everything.

### Retransmission semantics

Walk through a PalRle tile T scheduled with palette P at `delivered[P] = false`:

1. Phase A: `acquire_or_allocate(P) = Some(slot_5)`; `bundled = true`; `in_flight_carrying[5] += 1` → 1.
2. Phase B: `encode_pal_rle_payload(..., palette_id=5, bundled=true)` produces payload with palette block inline.
3. Phase C: `scheduler.enqueue(TileWork { codec: PalRle, payload, .. })`.
4. Datagrams sent. Tile T held as `InFlight` in scheduler.
5. **Case: client ACKs tile T.** `Scheduler::on_ack` resolves T; IoBridge callback: `delivered.insert(5)`; `in_flight_carrying[5] -= 1` → 0. All future tiles using slot 5 emit thin payloads.
6. **Case: 2×RTT retry fires.** Scheduler re-emits the same TileWork (same `bundled` flag because the payload is stored bytes, not regenerated). On eventual ACK, same as case 5.
7. **Case: tile T superseded mid-flight** (content change, `bump_generation`). Scheduler cancels T's TileWork; IoBridge callback fires (or scheduler exposes the cancelled work for cleanup): `in_flight_carrying[5] -= 1` → 0; `delivered[5]` unchanged. Some future tile using P bundles and eventually flips `delivered`.

At 50 ms RTT × 60 fps = 3 frames/RTT. With 2×RTT retry granularity and ACK batching (≤5 ms), realistic worst case is ~6–10 frames of bundled retransmission before a new palette is confirmed delivered. Acceptable.

---

## Wire format

`Codec::PalRle = 2` discriminant unchanged from M2. The per-codec payload that follows the `DatagramHeader` + `TileHeader` framing:

```
Offset  Field           Size   Notes
─────────────────────────────────────────────────────────────────────
[0]     flags           1 B    bit 0 = palette_bundled
                               bits 1..7 = reserved (must be 0; readers
                                            ignore unknown bits)

[1]     palette_id      1 B    persistent PaletteTable slot id (0..255)

if (flags & 0x01):              ─── bundled palette block ───
[2]     count           1 B    1..=16
[3..]   colors          c×4 B  BGRA, canonical BGRA-ascending order

[then]  rle_bytes       1..   nibble-packed RLE bytes:
                               high nibble = color index (0..count-1)
                               low nibble  = run_length - 1 (1..16 pixels)
                               pixels in row-major order (y * 32 + x);
                               runs cross row boundaries
```

### Size envelope

| Scenario | Bytes |
|---|---|
| Thin payload, fixed overhead | 2 |
| Bundled overhead (count=16) | 67 |
| RLE — best case (one color, 64 max-runs) | 64 |
| RLE — typical text (4–12 px runs) | ~150–300 |
| RLE — worst case (no run > 1 px) | 1024 |
| Bundled + worst-case RLE | 1091 |
| MTU budget (1200 datagram − 20 header) | 1180 |

Worst-case-bundled fits in a single datagram with ~90 bytes of headroom. If headroom dries up, existing `fragment_tile()` handles multi-fragment PalRLE transparently.

### Encode contract

`encode_pal_rle_payload(indices: &[u8; 512], palette: &PaletteEntry, persistent_id: u8, bundled: bool) -> Vec<u8>`:

1. Write `flags = if bundled { 0x01 } else { 0x00 }`.
2. Write `persistent_id`.
3. If `bundled`: write `palette.count`, then `palette.count × 4` bytes of `palette.colors`.
4. Walk the 1024 nibbles in `indices` (low nibble of byte 0 = pixel 0, high nibble of byte 0 = pixel 1, low nibble of byte 1 = pixel 2, …). For each pixel:
   - Compare to `current_index`. If different OR `current_run == 16`: emit `(current_index << 4) | (current_run - 1)`, reset.
   - Increment `current_run`.
5. Flush the final run.

### Decode contract

`decode_pal_rle(payload: &[u8], cached_palette: Option<&PaletteEntry>) -> Result<[[u8; 4]; 1024], DecodeError>` (server-side parity-test variant; client mirrors in TypeScript):

1. Read `flags`. If bundled: read `count`, palette inline. Else require `cached_palette = Some(...)`; error if `None`.
2. Walk `rle_bytes` decoding `(index, run+1)` pairs, emitting `run+1` copies of `palette.colors[index]`.
3. Bounds check: total emitted pixels must equal 1024 (`DecodeError::PixelCountMismatch`).
4. Bounds check: every `index < count` (`DecodeError::IndexOutOfRange`).

### Reserved bits — M3.2b parking

`flags` bit 1 reserved for a future `indices_raw` variant: payload after optional palette block is the raw 4-bit index stream (`[u8; 512]`), no RLE expansion needed client-side. Server emits whichever the client advertises support for in a future handshake. Reserved bit-space holds for M3.2b without touching the M3.2a wire.

---

## Classifier integration

### Pure interface preserved

`classify_tile(metrics: &TileMetrics, prev: &CodecState) -> CodecState` — signature unchanged. No palette-table view threaded in (D7).

### Role split

| Stage | Decides | Mutation |
|---|---|---|
| `classify_tile` (pure) | "is this tile PalRLE-feasible?" — Rules 3 and 7 on `unique_colors ≤ 16` | returns `CodecState::PalRle { palette_id: 0 }` placeholder |
| `decide_frame_mode` (pure) | "tile-codec or H.264 mode?" — sums per-tile cost using placeholder states | only `ClassifierHysteresis` counters |
| Phase A (CPU, serial) | "does the GPU-derived effective palette get a persistent slot? bundle or thin?" | overwrites `codec_state` with real `PalRle { palette_id: <real> }` on success, `Skip` on failure |
| Phase C dispatch | "what bytes go on the wire?" | reads `codec_state` for routing |

### `palette_id == 0` overload (D8)

`CodecState::PalRle { palette_id: u8 }` enum unchanged. The value `0` has two meanings depending on lifecycle position:

- **Pre-Phase-A** (after `classify_tile`, before Phase A overwrites): placeholder meaning "feasibility marker, to be allocated".
- **Post-Phase-A**: real persistent slot id `0`.

The boundary is clear in code — Phase A is the only thing that overwrites this field. Documented in the `CodecState::PalRle` doc-string.

### Hysteresis interaction

The classifier's hysteresis machinery (`Classifier::decide_frame_mode` counters; per-tile Rule 4 H264-stay) is read-only with respect to PalRle. Allocation failures cycling a tile through `PalRle → Skip → PalRle` across frames don't perturb any hysteresis counter — Rule 4 only checks `prev == H264`, and Rules 6/7 (Solid/PalRle) have no hysteresis component.

### Cost-model wiring

`CostModel::palrle_us` (~5 µs placeholder) and `estimated_tile_bytes(CodecState::PalRle { .. })` (~200 B placeholder) already exist (M3.0 scaffolding at `classifier.rs:61, 74`). M3.5 bench retunes from measurements. No M3.2a change to the structure; placeholder values are accurate enough that the frame-mode decision behaves reasonably.

### Proptest C-invariant impact

- **C3** (`unique_colors == 1 ⇒ CodecState::Solid`) — already re-enabled in M3.1; unaffected.
- **C7** (`unique_colors ≤ 16 ⇒ PalRle`) — **newly re-enabled in M3.2a** alongside encoder landing.
- Other invariants unaffected.

---

## Client-side CPU decode (M3.2a)

### TypeScript only, no WASM (D10)

The actual workload — 1024 pixels × ~5 ns/pixel pure-JS RLE expansion — is ~5 µs per tile, ~500 µs/frame at ~100 PalRLE tiles, well under frame budget. WASM toolchain investment for code WebGPU will replace in M3.2b is wasted infrastructure. Pure TS for M3.2a; profile-driven only if proven needed.

### New module — `ghostframe-web-client/src/palette.ts`

```ts
export type PaletteId = number;         // 0..255
export type Bgra = readonly [number, number, number, number];

export class PaletteCache {
    private entries = new Map<PaletteId, Bgra[]>();

    upsert(id: PaletteId, colors: Bgra[]): void {
        this.entries.set(id, colors);
    }

    get(id: PaletteId): Bgra[] | undefined {
        return this.entries.get(id);
    }

    clear(): void {
        this.entries.clear();
    }
}
```

No ref counting, no LRU, no delivered tracking — the client's cache is unconditional storage keyed by palette_id. The server's `PaletteTable` carries all lifecycle machinery.

### `decoder.ts` extension — PalRle branch

```ts
export function decodePalRle(
    payload: Uint8Array,
    cache: PaletteCache,
): { bgra: Uint8ClampedArray; updatedPaletteId?: PaletteId } {
    let cursor = 0;
    const flags = payload[cursor++];
    const paletteId = payload[cursor++];
    const bundled = (flags & 0x01) !== 0;

    let palette: Bgra[];
    let updated: PaletteId | undefined;

    if (bundled) {
        const count = payload[cursor++];
        palette = new Array(count);
        for (let i = 0; i < count; i++) {
            palette[i] = [
                payload[cursor++], payload[cursor++],
                payload[cursor++], payload[cursor++],
            ];
        }
        cache.upsert(paletteId, palette);
        updated = paletteId;
    } else {
        const cached = cache.get(paletteId);
        if (!cached) {
            throw new DecodeError(`PalRle thin payload references uncached palette ${paletteId}`);
        }
        palette = cached;
    }

    const bgra = new Uint8ClampedArray(32 * 32 * 4);
    let pixelIdx = 0;
    while (cursor < payload.length) {
        const b = payload[cursor++];
        const colorIdx = b >> 4;
        const runLen = (b & 0x0F) + 1;
        if (colorIdx >= palette.length) {
            throw new DecodeError(`palette index ${colorIdx} >= count ${palette.length}`);
        }
        const [bb, gg, rr, aa] = palette[colorIdx];
        // Force alpha = 255 to work around X11 BGRX alpha quirk
        // (see feedback memory: canvas premultiplied alpha)
        const aOut = aa === 0 ? 255 : aa;
        for (let i = 0; i < runLen; i++) {
            const offset = pixelIdx * 4;
            bgra[offset    ] = bb;
            bgra[offset + 1] = gg;
            bgra[offset + 2] = rr;
            bgra[offset + 3] = aOut;
            pixelIdx++;
        }
    }

    if (pixelIdx !== 1024) {
        throw new DecodeError(`PalRle decoded ${pixelIdx} pixels, expected 1024`);
    }
    return { bgra, updatedPaletteId: updated };
}
```

### `renderer.ts` extension — `drawPalRleTile`

```ts
export function drawPalRleTile(
    ctx: CanvasRenderingContext2D,
    tileX: number,
    tileY: number,
    bgra: Uint8ClampedArray,
): void {
    const imageData = new ImageData(bgra, 32, 32);
    ctx.putImageData(imageData, tileX * 32, tileY * 32);
}
```

Reuses the same path Raw uses today.

### `main.ts` integration

The reassembly callback already routes by `Codec`. Add a `PaletteCache` instance alongside the existing `H264TileDecoder` map. On WebTransport session close: `paletteCache.clear()`. On reconnect: fresh empty cache; server's `on_session_reset` ensures all next emissions bundle.

### Error path — thin-without-cache

`decodePalRle` throws `DecodeError`; main.ts logs and drops the tile. Server's 2×RTT retry re-bundles on the next pass. Self-recovering.

---

## Error handling, edge cases

| Failure | Where detected | Fallback | Wire effect |
|---|---|---|---|
| Palette-table full at Phase A | CPU | per-tile Raw; `codec_state → Skip` | tile emits as `Codec::Raw`, ~4 KB instead of ~200 B |
| Frame-palette-set overflow (>256 unique in one frame) | Stage 2a `atomicCAS` probe-budget exhaustion | `per_tile_frame_palette_id[c] = 0xFF`; Phase A treats as table-full | same as above |
| Vulkan device error in Stages 1.5/2a/2b/3 | `process_frame_gpu` returns Err | fall through to `process_frame_cpu` (Raw-only) | entire frame Raw — degraded but correct |
| MTU overflow on bundled-worst-case payload | `fragment_tile` | existing multi-fragment split | two datagrams instead of one; transparent |
| Generation wrap (4-bit) | Scheduler (M3.1) | supersession cancels old-gen tiles | none — safe under 2×RTT retry |
| Tile superseded carrying bundled palette | Scheduler callback | `in_flight_carrying[id] -= 1`; `delivered[id]` unchanged | future tile re-bundles |
| Session disconnect mid-frame | post-fence detection | drop frame (M3.1 behavior) | none |
| Session takeover | new WT session accept | `scheduler.clear()` + `palette_table.on_session_reset()` | first post-reset emissions bundle |
| Client thin-payload-without-cache | `decoder.ts` `decodePalRle` | throw, log, drop tile | server's 2×RTT retry re-bundles |
| Client pixel-count mismatch / index out of range | `decoder.ts` | throw, log, drop tile | same |

### Counter underflow safety

`palette_table.in_flight_carrying[id]` is `u32` driven by paired enqueue/ack/supersession events. Pairing is not externally enforceable. Use `saturating_sub(1)` for decrements; log `tracing::warn` if pre-decrement value was 0. Misuse becomes loud-but-non-fatal.

### Explicitly out-of-scope for M3.2a

- Adversarial palette-flooding (single-trusted-client deployment model).
- Multi-client correctness (single-client invariant — D3).
- Client decode error feedback to server (M3.2b candidate).

### Observability

```rust
tracing::debug!(target = "palrle.frame", ...);  // every frame stats summary
tracing::warn!(target = "palrle.alloc",  ...);  // every allocation failure
```

Client decode errors logged client-side; no server-side mirror in M3.2a.

---

## Testing strategy

### Unit tests — pure CPU

| Module | Test | Coverage |
|---|---|---|
| `encoder/pal_rle.rs` | `encode → decode` roundtrip on synthetic index buffers | bit-exact lossless contract |
| `encoder/pal_rle.rs` | `encode_pal_rle_payload` bundled vs thin shape | flag byte, palette block presence |
| `encoder/pal_rle.rs` | `PaletteTable::find_matching` hits both Held and FreeButCached | umbrella semantics |
| `encoder/pal_rle.rs` | `acquire_or_allocate` 4-way ladder | full match → eligible-free → empty → fail |
| `encoder/pal_rle.rs` | `overwrite_eligible` predicate truth table | D4 collision-prevention rule |
| `encoder/pal_rle.rs` | `on_session_reset` preserves byte cache, clears tracking | D3 warm-cache invariant |
| `encoder/pal_rle.rs` | `in_flight_carrying` saturating-sub on underflow | counter safety |
| `transport/io_bridge.rs` | Phase A serial palette assignment with mocked GPU output | classifier → palette-table → preps |
| `transport/io_bridge.rs` | Phase B → C dispatch under all four allocation outcomes | (match, allocate-new, allocate-overwrite, fail) |
| `transport/io_bridge.rs` | ACK callback flips `delivered`, decrements `in_flight_carrying` | D12 seam |
| `transport/io_bridge.rs` | Supersession callback decrements without flipping `delivered` | D12 seam |

### Property tests

| Test | Invariant |
|---|---|
| Re-enable **C7** | `unique_colors ≤ 16 ⇒ CodecState::PalRle` |
| `encode_pal_rle ↔ decode_pal_rle` | bit-exact identity on arbitrary palette + arbitrary index streams |
| `PaletteTable` op-sequence invariants | `ref_count` consistency under any allocate/acquire/release sequence |
| Subset-fold correctness | if `folded_into[A] = B`, decoding A's pixels against B = decoding against A |

### GPU integration tests

Pattern extends `gpu_pipeline.rs`'s existing `process_frame_returns_tile_analysis_for_*` tests:

| Test | Asserts |
|---|---|
| `process_frame_returns_palrle_compact_list` | Stage 1.5: dirty + count≤16 tiles listed; others filtered |
| `process_frame_dedups_identical_palettes_within_frame` | Stage 2a: 4 tiles with identical 4-color palette → 1 frame palette |
| `process_frame_folds_subset_palette` | Stage 2b: `{R,G,B}` ⊆ `{R,G,B,W}` → `folded_into[{R,G,B}] = {R,G,B,W}` |
| `process_frame_emits_correct_indices_for_two_color_tile` | Stage 3: half-and-half tile → 512 indices, half 0x00 half 0x11 |
| `process_frame_handles_palette_overflow_via_sentinel` | adversarial 300-distinct-palette frame → some `0xFF` sentinels, Phase A falls to Raw |
| `process_frame_canonical_sort_is_stable` | Stage 1 sort extension: arbitrary input → BGRA-ascending output |

### E2E tests

| Test | Status | Asserts |
|---|---|---|
| `e2e_text_clarity` | extend existing | protocol-layer inspector confirms `Codec::PalRle = 2` on tile datagrams; payload < N bytes (target ~250 B/tile vs Raw 4 KB); canvas pixel-equality (SSIM = 1.0) |
| `e2e_palette_eviction` | new | test pattern draws ~300 sequentially-distinct text regions; server logs show palette slot reuse not allocation-failure; rendering correctness preserved |
| `e2e_palrle_5pct_loss` | new | 5% outbound datagram drop; text region eventually renders correctly via 2×RTT retries; bundled retransmits arrive before client times out |
| `e2e_palrle_session_reset` | new | force session reconnect mid-stream; next-frame emissions all bundled; warm-cache hits via `find_matching` produce correct rendering |

### Bench harness — M3.5 enabler

Per umbrella M3.5: a `BenchEncoder` impl for PalRLE in `ghostframe-lib/benches/codec_latency.rs`. Inputs from existing content-class harness (text, gradient, photo, mixed). Outputs through LZ4-on/off branches. M3.2a ships the impl; M3.5 runs the numbers. ~30 LoC.

### Loss-injection coverage

M3.1 env-var protocol extends with `palrle_bundled` / `palrle_thin` predicates (~10 LoC).

### Explicitly NOT tested in M3.2a

- WASM decode parity (no WASM ships).
- WebGPU client decode (M3.2b).
- BC1 fallback path (M3.4).
- Multi-client palette correctness (single-client invariant).
- Cross-hardware GPU bench portability (M3.5 future work).

---

## M3.2b parking lot

### Deferred to M3.2b

| Item | Notes |
|---|---|
| Client renderer migration to WebGPU canvas | largest single line item; touches every codec path |
| `PaletteCache` → palette atlas `GPUTexture` | data-structure change; semantics preserved |
| Wire-format `flags` bit 1 — `indices_raw` variant | server emits 512 B raw 4-bit indices when client advertises support; capability negotiation needed |
| Client decode error feedback to server | new field in `ReceiverFeedback` carrying recent decode-error counts |
| WASM module (umbrella's original deliverable) | likely **skipped entirely** — WebGPU compute supersedes |

### Deferred but not M3.2b-coupled (M3.5-data-driven)

| Item | When to reconsider |
|---|---|
| GPU subset-fold against persistent-table palettes | after M3.5 bench shows persistent reuse is a meaningful bandwidth fraction |
| Approach-A pure-GPU RLE byte-stream encode | after M3.5 bench shows Stage 4 CPU time is meaningful |
| `pulp` inner-loop SIMD on RLE encode | after M3.5 bench |
| Persistent-table cap-based churn limiting | after observing pathological `stats_frame.fell_back_to_raw` |
| `TileAnalysis` struct split (`colors[16]` → DEVICE_LOCAL) | only if mapped-buffer overhead shows up in profiling |

### Stays valid from M3.2a into M3.2b — do not change

| Component | Reason |
|---|---|
| Server-side GPU pipeline (Stages 1.5, 2a, 2b, 3) | server is client-agnostic |
| Stage 4 CPU pipeline | server-side, client-agnostic |
| `PaletteTable` lifecycle | pure server-side state |
| `delivered` BitSet + `in_flight_carrying` counter | server-side correctness machinery |
| Wire format flags bit 0, palette_id, palette block layout, nibble-RLE stream | stable; bit 1 is additive |
| `Codec::PalRle = 2` discriminant + `TileHeader` framing | unchanged |
| Scheduler + ACK protocol | unchanged from M3.1 |
| `CodecState::PalRle { palette_id }` + classifier rules (incl. C7) | unchanged |
| `CostModel` structure | retuned by M3.5; shape stable |
| M3.2a unit tests, proptests, E2E tests | stay green; M3.2b adds GPU-decode assertions without replacement |

### Post-M3.2 phase ordering

The umbrella's M3 phase list shifts slightly:

```
M3.2a (this session)  ─►  server GPU encode + CPU client decode
        ↓
M3.2b (next session)  ─►  client WebGPU migration (every codec, not just PalRle)
        ↓
M3.3                   ─►  CDF 5/3 + multi-pass refinement (umbrella unchanged)
        ↓
M3.4                   ─►  BC1 (conditional per M3.5 dominance check)
        ↓
M3.5                   ─►  bench publication + threshold tuning + BC1 fate
```

M3.2b can land before or after M3.3 — they're independent.

---

## Document pointers

- M3 umbrella: `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` (§M3.2 envelope this design supersedes for the specifics)
- M3.1 addendum: `docs/superpowers/specs/2026-05-11-m3.1-solid-scheduler-decisions.md` (scheduler + ACK + dispatch helper)
- GPU tile-analysis: `docs/superpowers/specs/2026-05-11-gpu-tile-analysis-design.md` (Stage 1 substrate)
- Memory notes:
  - `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m31_deferred_budget.md` — `Scheduler::tick` real budgeting still placeholder, lands with M3.3
  - `~/.claude/projects/-home-cedric-work-ghostframe/memory/reference_e2e_loss_injection.md` — env-var protocol used by `e2e_palrle_5pct_loss`
  - `~/.claude/projects/-home-cedric-work-ghostframe/memory/feedback_canvas_alpha.md` — X11 BGRX alpha=0 fix applied in `decodePalRle`
