// cdf53_integrate.wgsl — bit-plane OR-integration with generation-bump self-clear.
//
// One workgroup per queued pass (dispatchWorkgroups(batchSize)). The workgroup
// of 64 invocations strides over the tile's 3 × 1024 coefficient lanes.
//
// Per workgroup:
//   1. Invocation 0 detects generation bump → atomicStore new gen + sets a
//      workgroup-shared "should_clear" flag. workgroupBarrier.
//   2. If should_clear, all 64 invocations stride-clear the tile's
//      coefficient + sign buffer slots. workgroupBarrier.
//   3. All invocations OR this pass's bit-plane into the persistent buffers:
//      - pass_idx == 0 ⇒ sign plane → atomicOr into signBuffer.
//      - pass_idx > 0  ⇒ magnitude bit `13 - pass_idx` → atomicOr into
//                       coefficientBuffer (packed two i16 per u32).
//   4. Invocation 0 atomicAdd's the tile index into dirtyTiles.

struct TileWorkEntry {
  tile_x: u32,
  tile_y: u32,
  gen: u32,
  pass_idx: u32,
  bit_planes_offset: u32, // u32 index into bitPlanes (96 u32 per pass)
};

struct Uniforms {
  cols: u32,
  _pad: vec3<u32>,
};

@group(0) @binding(0) var<storage, read> tileWork: array<TileWorkEntry>;
@group(0) @binding(1) var<storage, read> bitPlanes: array<u32>;
@group(0) @binding(2) var<storage, read_write> coefficientBuffer: array<atomic<u32>>;
@group(0) @binding(3) var<storage, read_write> signBuffer: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> tileGen: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read_write> dirtyTiles: array<u32>;
@group(0) @binding(6) var<storage, read_write> dirtyTilesCount: atomic<u32>;
@group(0) @binding(7) var<uniform> uniforms: Uniforms;

var<workgroup> should_clear: u32;

@compute @workgroup_size(64)
fn main(@builtin(workgroup_id) wg: vec3<u32>,
        @builtin(local_invocation_index) lid: u32) {
  let entry = tileWork[wg.x];
  let tile_idx = entry.tile_y * uniforms.cols + entry.tile_x;

  // 1. Gen-bump detection.
  if (lid == 0u) {
    let prev = atomicLoad(&tileGen[tile_idx]);
    if (prev != entry.gen) {
      atomicStore(&tileGen[tile_idx], entry.gen);
      should_clear = 1u;
    } else {
      should_clear = 0u;
    }
  }
  workgroupBarrier();

  // 2. Conditional clear (gen bumped).
  if (should_clear != 0u) {
    // coefficientBuffer: 1536 u32 per tile.
    for (var i: u32 = lid; i < 1536u; i = i + 64u) {
      atomicStore(&coefficientBuffer[tile_idx * 1536u + i], 0u);
    }
    // signBuffer: 96 u32 per tile.
    for (var i: u32 = lid; i < 96u; i = i + 64u) {
      atomicStore(&signBuffer[tile_idx * 96u + i], 0u);
    }
    workgroupBarrier();
  }

  // 3. Bit-plane integration.
  for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
    let plane_base = entry.bit_planes_offset + ch * 32u;
    for (var i: u32 = lid; i < 1024u; i = i + 64u) {
      let word = bitPlanes[plane_base + (i >> 5u)];
      let bit = (word >> (i & 31u)) & 1u;
      if (bit == 0u) { continue; }
      if (entry.pass_idx == 0u) {
        // Sign plane: set bit i in signBuffer[tile_idx*96 + ch*32 + i/32].
        let word_idx = tile_idx * 96u + ch * 32u + (i >> 5u);
        atomicOr(&signBuffer[word_idx], 1u << (i & 31u));
      } else {
        // Magnitude bit_pos = 13 - pass_idx.
        let bit_pos = 13u - entry.pass_idx;
        // Pack: two i16 per u32. i even ⇒ low half, i odd ⇒ high half.
        let lane_value: u32 = 1u << bit_pos;
        let shifted = select(lane_value, lane_value << 16u, (i & 1u) == 1u);
        // Layout: coefficientBuffer[tile_idx*1536 + ch*512 + i/2].
        let word_idx = tile_idx * 1536u + ch * 512u + (i >> 1u);
        atomicOr(&coefficientBuffer[word_idx], shifted);
      }
    }
  }

  // 4. Emit tile_idx into dirtyTiles list (one per workgroup).
  if (lid == 0u) {
    let slot = atomicAdd(&dirtyTilesCount, 1u);
    if (slot < arrayLength(&dirtyTiles)) {
      dirtyTiles[slot] = tile_idx;
    }
  }
}
