// cdf53_inverse_l2.wgsl — middle-level inverse lifting (16×16 working area).
//
// Reads the L3-inverse output from workArea[0..8][0..8] (LL2 — the spatial
// reconstruction of the 8×8 LL3+detail block produced by inverse_l3), and the
// HL2/LH2/HH2 detail subbands from coefficientBuffer (still bit-packed signed
// i16 magnitudes + signBuffer), then runs one level of inverse 2D lifting on
// the 16×16 area.
//
// One workgroup per dirty tile, @workgroup_size(16, 16, 1) = 256 invocations.

@group(0) @binding(0) var<storage, read> coefficientBuffer: array<u32>;
@group(0) @binding(1) var<storage, read> signBuffer: array<u32>;
@group(0) @binding(2) var<storage, read> tileGen: array<u32>;
@group(0) @binding(3) var<storage, read_write> workArea: array<i32>;

fn unpack_i16(word: u32, lane: u32) -> i32 {
  let raw = (word >> (lane * 16u)) & 0xFFFFu;
  return select(i32(raw), i32(raw) - 0x10000, (raw & 0x8000u) != 0u);
}
fn read_coeff(tile_idx: u32, ch: u32, i: u32) -> i32 {
  let word_idx = tile_idx * 1536u + ch * 512u + (i >> 1u);
  let mag = unpack_i16(coefficientBuffer[word_idx], i & 1u);
  let sign_word_idx = tile_idx * 96u + ch * 32u + (i >> 5u);
  let sign_bit = (signBuffer[sign_word_idx] >> (i & 31u)) & 1u;
  return select(mag, -mag, sign_bit != 0u);
}
fn workarea_idx(tile_idx: u32, ch: u32, y: u32, x: u32) -> u32 {
  return tile_idx * 3072u + ch * 1024u + y * 32u + x;
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
  let tile_idx = wg.x;
  if (tileGen[tile_idx] == 0u) { return; }

  // Channel layout (per cdf53.rs):
  //   HL2 8×8 → ch*1024 + [64..128)
  //   LH2 8×8 → ch*1024 + [128..192)
  //   HH2 8×8 → ch*1024 + [192..256)
  // Map (y, x) in 0..16 → either LL2 (top-left 8×8, already in workArea) or
  // one of the three detail subbands (read from coefficientBuffer).

  for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
    let y = lid.y;
    let x = lid.x;
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
  workgroupBarrier();

  // Inverse vertical pass for 16×16: each column (lid.x runs 0..16).
  if (lid.y == 0u) {
    for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
      let col = lid.x;
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

  // Inverse horizontal pass.
  if (lid.x == 0u) {
    for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
      let row = lid.y;
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
