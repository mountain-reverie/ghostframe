// cdf53_inverse_l2.wgsl — middle-level inverse lifting (16×16 working area).
//
// Reads the L3-inverse output from workArea[0..8][0..8] (LL2 — the spatial
// reconstruction of the 8×8 LL3+detail block produced by inverse_l3), and the
// HL2/LH2/HH2 detail subbands from coefficientBuffer (still bit-packed signed
// i16 magnitudes + signBuffer), then runs one level of inverse 2D lifting on
// the 16×16 area.
//
// One workgroup per dirty tile, @workgroup_size(16, 8, 1) = 128 invocations.
//
// 128, not 256: GLES 3.1's maxComputeInvocationsPerWorkgroup minimum is 128
// and Mali-T860 reports exactly that, so at 256 the native client cannot
// create a device at all on Midgard (GLES/V4L2 design §5). WebGPU guarantees
// 256, so 128 is valid in every browser too. Enforced by
// `shader_validation.rs::no_compute_shader_exceeds_the_portable_workgroup_limit`.
//
// The load phase gives each invocation two rows. The two lifting passes need
// exactly 16 workers each and select them with a LINEAR thread id, not with
// `lid.y == 0` / `lid.x == 0`: the horizontal pass indexes its row by the
// worker number, so keying it off `lid.y` silently processed only rows
// 0..8 once the y dimension halved -- a tile whose lower half never got the
// horizontal inverse, which is wrong pixels rather than a crash. Keeping the
// guards independent of the workgroup shape means the next reshape cannot
// reintroduce that.

@group(0) @binding(0) var<storage, read> coefficientBuffer: array<u32>;
@group(0) @binding(1) var<storage, read> signBuffer: array<u32>;
@group(0) @binding(2) var<storage, read> tileGen: array<u32>;
@group(0) @binding(3) var<storage, read_write> workArea: array<i32>;
@group(0) @binding(4) var<storage, read> passesProcessed: array<u32>;

fn unpack_i16(word: u32, lane: u32) -> i32 {
  let raw = (word >> (lane * 16u)) & 0xFFFFu;
  return select(i32(raw), i32(raw) - 0x10000, (raw & 0x8000u) != 0u);
}
fn read_coeff(tile_idx: u32, ch: u32, i: u32) -> i32 {
  let word_idx = tile_idx * 1536u + ch * 512u + (i >> 1u);
  let raw_mag = unpack_i16(coefficientBuffer[word_idx], i & 1u);
  let sign_word_idx = tile_idx * 96u + ch * 32u + (i >> 5u);
  let sign_bit = (signBuffer[sign_word_idx] >> (i & 31u)) & 1u;
  // Midpoint reconstruction — see cdf53_inverse_l3.wgsl for the rationale.
  var mag = raw_mag;
  let passes = passesProcessed[tile_idx];
  if (raw_mag != 0 && passes >= 2u && passes < 14u) {
    let unknown_bits = 14u - passes;
    let midpoint = i32(1u << (unknown_bits - 1u));
    mag = raw_mag + midpoint;
  }
  return select(mag, -mag, sign_bit != 0u);
}
fn workarea_idx(tile_idx: u32, ch: u32, y: u32, x: u32) -> u32 {
  return tile_idx * 3072u + ch * 1024u + y * 32u + x;
}

@compute @workgroup_size(16, 8, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
  let tile_idx = wg.x;
  // Uniform across the workgroup (tile_idx is wg.x), so every invocation
  // returns together and the barriers below stay uniform.
  if (tileGen[tile_idx] == 0u) { return; }

  // 0..128. The lifting passes below pick their 16 workers from this rather
  // than from a single `lid` component, so they do not depend on how the 128
  // invocations are split between x and y.
  let tid = lid.y * 16u + lid.x;

  // Channel layout (per cdf53.rs):
  //   HL2 8×8 → ch*1024 + [64..128)
  //   LH2 8×8 → ch*1024 + [128..192)
  //   HH2 8×8 → ch*1024 + [192..256)
  // Map (y, x) in 0..16 → either LL2 (top-left 8×8, already in workArea) or
  // one of the three detail subbands (read from coefficientBuffer).

  // Two rows per invocation: lid.y runs 0..8 and covers y 0..16. No barrier
  // and no return inside, so this is a pure re-indexing of the same work.
  for (var yy: u32 = 0u; yy < 2u; yy = yy + 1u) {
    let y = lid.y * 2u + yy;
    let x = lid.x;
    for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
      let in_top = y < 8u;
      let in_left = x < 8u;
      if (in_top && in_left) {
        // LL2 — already in workArea from inverse_l3. Nothing to do.
      } else {
        let qx = select(x - 8u, x, in_left);
        let qy = select(y - 8u, y, in_top);
        let base = select(
          select(192u, 128u, in_left), // bottom: HH2 (right) or LH2 (left)
          64u,                         // top right: HL2 (no left case here because in_top && in_left handled)
          in_top
        );
        let i = base + qy * 8u + qx;
        workArea[workarea_idx(tile_idx, ch, y, x)] = read_coeff(tile_idx, ch, i);
      }
    }
  }
  workgroupBarrier();

  // Inverse vertical pass for 16×16: one worker per column, 16 of them.
  if (tid < 16u) {
    for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
      let col = tid;
      var tmp: array<i32, 16>;
      for (var k: u32 = 0u; k < 8u; k = k + 1u) {
        tmp[2u * k] = workArea[workarea_idx(tile_idx, ch, k, col)];
        tmp[2u * k + 1u] = workArea[workarea_idx(tile_idx, ch, 8u + k, col)];
      }
      for (var k: u32 = 0u; k < 16u; k = k + 2u) {
        let left = select(tmp[k - 1u], tmp[1], k == 0u);
        let right = select(tmp[15], tmp[k + 1u], k + 1u < 16u);
        tmp[k] = tmp[k] - ((left + right + 2) >> 2);
      }
      for (var k: u32 = 1u; k < 16u; k = k + 2u) {
        let left = tmp[k - 1u];
        let right = select(tmp[k - 1u], tmp[k + 1u], k + 1u < 16u);
        tmp[k] = tmp[k] + ((left + right) >> 1);
      }
      for (var k: u32 = 0u; k < 16u; k = k + 1u) {
        workArea[workarea_idx(tile_idx, ch, k, col)] = tmp[k];
      }
    }
  }
  workgroupBarrier();

  // Inverse horizontal pass: one worker per row, 16 of them.
  //
  // This is the guard that used to read `lid.y` and index `row` by it. With
  // @workgroup_size(16, 8) that covers rows 0..8 only, leaving the bottom
  // half of every tile without its horizontal inverse pass.
  if (tid < 16u) {
    for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
      let row = tid;
      var tmp: array<i32, 16>;
      for (var k: u32 = 0u; k < 8u; k = k + 1u) {
        tmp[2u * k] = workArea[workarea_idx(tile_idx, ch, row, k)];
        tmp[2u * k + 1u] = workArea[workarea_idx(tile_idx, ch, row, 8u + k)];
      }
      for (var k: u32 = 0u; k < 16u; k = k + 2u) {
        let left = select(tmp[k - 1u], tmp[1], k == 0u);
        let right = select(tmp[15], tmp[k + 1u], k + 1u < 16u);
        tmp[k] = tmp[k] - ((left + right + 2) >> 2);
      }
      for (var k: u32 = 1u; k < 16u; k = k + 2u) {
        let left = tmp[k - 1u];
        let right = select(tmp[k - 1u], tmp[k + 1u], k + 1u < 16u);
        tmp[k] = tmp[k] + ((left + right) >> 1);
      }
      for (var k: u32 = 0u; k < 16u; k = k + 1u) {
        workArea[workarea_idx(tile_idx, ch, row, k)] = tmp[k];
      }
    }
  }
}
