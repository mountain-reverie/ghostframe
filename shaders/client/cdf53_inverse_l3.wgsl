// cdf53_inverse_l3.wgsl — innermost-level inverse lifting (8×8 working area).
//
// Reads sign+magnitude from persistent buffers, assembles signed i32
// coefficients in the LL3+HL3+LH3+HH3 quadrants (top-left 8×8 of workArea),
// and runs one level of inverse 2D CDF 5/3 lifting in place.
//
// One workgroup per dirty tile, @workgroup_size(8, 8, 1) = 64 invocations.

@group(0) @binding(0) var<storage, read> coefficientBuffer: array<u32>;
@group(0) @binding(1) var<storage, read> signBuffer: array<u32>;
@group(0) @binding(2) var<storage, read> tileGen: array<u32>;
@group(0) @binding(3) var<storage, read_write> workArea: array<i32>;
@group(0) @binding(4) var<storage, read> passesProcessed: array<u32>;

fn unpack_i16(word: u32, lane: u32) -> i32 {
  // lane = 0 ⇒ low half, lane = 1 ⇒ high half. Sign-extend 16 → 32.
  let raw = (word >> (lane * 16u)) & 0xFFFFu;
  // Sign-extend.
  return select(i32(raw), i32(raw) - 0x10000, (raw & 0x8000u) != 0u);
}

fn read_coeff(tile_idx: u32, ch: u32, i: u32) -> i32 {
  let word_idx = tile_idx * 1536u + ch * 512u + (i >> 1u);
  let raw_mag = unpack_i16(coefficientBuffer[word_idx], i & 1u);
  let sign_word_idx = tile_idx * 96u + ch * 32u + (i >> 5u);
  let sign_bit = (signBuffer[sign_word_idx] >> (i & 31u)) & 1u;
  // Midpoint reconstruction (SPIHT-style): for partial-K decode where
  // K = passesProcessed[tile_idx] ∈ [2, 13], add 2^(13-K) to every
  // significant coefficient (raw_mag != 0) to estimate the unknown low
  // bits as the midpoint of their range. K < 2 means we only have signs
  // (or nothing) — no significant coefficients yet. K >= 14 means
  // lossless — no correction needed. Mirrors Rust `cdf53::decode_passes`.
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
  // 32 × 32 × i32 per channel; 3 channels per tile.
  return tile_idx * 3072u + ch * 1024u + y * 32u + x;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
  let tile_idx = wg.x;
  if (tileGen[tile_idx] == 0u) { return; }

  // Each invocation handles one (x, y) in the 8×8 LL3+detail area, for one of
  // 3 channels (looped). The channel layout per cdf53.rs:35-101:
  //   LL3 4×4 → coefficient slots ch*1024 + [0..16)
  //   HL3 4×4 → coefficient slots ch*1024 + [16..32)
  //   LH3 4×4 → coefficient slots ch*1024 + [32..48)
  //   HH3 4×4 → coefficient slots ch*1024 + [48..64)
  // Output in workArea[y][x] for y,x in 0..8 — quadrants packed:
  //   work[0..4][0..4]   = LL3
  //   work[0..4][4..8]   = HL3
  //   work[4..8][0..4]   = LH3
  //   work[4..8][4..8]   = HH3

  for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
    let y = lid.y;
    let x = lid.x;
    // Map (y, x) → coefficient index inside the channel.
    let in_top = y < 4u;
    let in_left = x < 4u;
    let qx = select(x - 4u, x, in_left);
    let qy = select(y - 4u, y, in_top);
    let quadrant_base = select(
      select(48u, 32u, in_left), // bottom row: HH3 (right) or LH3 (left)
      select(16u, 0u,  in_left), // top row:    HL3 (right) or LL3 (left)
      in_top
    );
    let i = quadrant_base + qy * 4u + qx;
    let coeff = read_coeff(tile_idx, ch, i);
    workArea[workarea_idx(tile_idx, ch, y, x)] = coeff;
  }
  workgroupBarrier();

  // Inverse vertical pass then inverse horizontal pass on the 8×8 area.
  // For each row/col we re-interleave [L L L L H H H H] → [L H L H ...] and
  // then run inverse update + inverse predict (mirror boundaries).

  // Vertical (per column): only invocations where lid.x is in [0,8) participate;
  // all do because workgroup is 8×8. lid.y < 4 reads the L half; the others read H.
  if (lid.y == 0u) {
    for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
      let col = lid.x;
      // Read L and H halves, write interleaved.
      var tmp: array<i32, 8>;
      for (var k: u32 = 0u; k < 4u; k = k + 1u) {
        tmp[2u * k] = workArea[workarea_idx(tile_idx, ch, k, col)];
        tmp[2u * k + 1u] = workArea[workarea_idx(tile_idx, ch, 4u + k, col)];
      }
      // Inverse update (even indices).
      for (var k: u32 = 0u; k < 8u; k = k + 2u) {
        let left = select(tmp[k - 1u], tmp[1], k == 0u);
        let right = select(tmp[7], tmp[k + 1u], k + 1u < 8u);
        tmp[k] = tmp[k] - ((left + right + 2) >> 2);
      }
      // Inverse predict (odd indices).
      for (var k: u32 = 1u; k < 8u; k = k + 2u) {
        let left = tmp[k - 1u];
        let right = select(tmp[k - 1u], tmp[k + 1u], k + 1u < 8u);
        tmp[k] = tmp[k] + ((left + right) >> 1);
      }
      // Write back.
      for (var k: u32 = 0u; k < 8u; k = k + 1u) {
        workArea[workarea_idx(tile_idx, ch, k, col)] = tmp[k];
      }
    }
  }
  workgroupBarrier();

  // Horizontal (per row): only lid.x == 0 participates.
  if (lid.x == 0u) {
    for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
      let row = lid.y;
      var tmp: array<i32, 8>;
      for (var k: u32 = 0u; k < 4u; k = k + 1u) {
        tmp[2u * k] = workArea[workarea_idx(tile_idx, ch, row, k)];
        tmp[2u * k + 1u] = workArea[workarea_idx(tile_idx, ch, row, 4u + k)];
      }
      for (var k: u32 = 0u; k < 8u; k = k + 2u) {
        let left = select(tmp[k - 1u], tmp[1], k == 0u);
        let right = select(tmp[7], tmp[k + 1u], k + 1u < 8u);
        tmp[k] = tmp[k] - ((left + right + 2) >> 2);
      }
      for (var k: u32 = 1u; k < 8u; k = k + 2u) {
        let left = tmp[k - 1u];
        let right = select(tmp[k - 1u], tmp[k + 1u], k + 1u < 8u);
        tmp[k] = tmp[k] + ((left + right) >> 1);
      }
      for (var k: u32 = 0u; k < 8u; k = k + 1u) {
        workArea[workarea_idx(tile_idx, ch, row, k)] = tmp[k];
      }
    }
  }
}
