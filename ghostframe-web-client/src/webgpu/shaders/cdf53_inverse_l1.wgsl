// cdf53_inverse_l1.wgsl — outermost-level inverse lifting (32×32 area).
//
// Reads the L2-inverse output from workArea[0..16][0..16] (LL1) plus the
// HL1/LH1/HH1 detail subbands from coefficientBuffer, runs one level of
// inverse 2D lifting on the 32×32 area, and writes reconstructed BGR
// pixels into the framebuffer storage texture (alpha = 1.0).
//
// Four workgroups per dirty tile (one per 16×16 quadrant of the 32×32 tile),
// @workgroup_size(16, 16, 1) = 256 invocations.

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

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
  let tile_slot = wg.x / 4u;
  let quadrant = wg.x % 4u;
  let tile_idx = tile_slot;
  if (tileGen[tile_idx] == 0u) { return; }

  // Per-channel layout for L1 detail:
  //   HL1 16×16 → ch*1024 + [256..512)
  //   LH1 16×16 → ch*1024 + [512..768)
  //   HH1 16×16 → ch*1024 + [768..1024)
  // Each quadrant of the 32×32 area covers one 16×16 region. We populate
  // workArea[y][x] for y,x in 0..32; only one quadrant per workgroup.

  let qy = quadrant / 2u;
  let qx = quadrant % 2u;
  let y_base = qy * 16u;
  let x_base = qx * 16u;
  let y = y_base + lid.y;
  let x = x_base + lid.x;

  for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
    let in_top = y < 16u;
    let in_left = x < 16u;
    var v: i32 = 0;
    if (in_top && in_left) {
      v = workArea[workarea_idx(tile_idx, ch, y, x)]; // LL1 — preserved from L2 step
    } else {
      let dy = select(y - 16u, y, in_top);
      let dx = select(x - 16u, x, in_left);
      let base = select(
        select(768u, 512u, in_left), // bottom: HH1 (right) or LH1 (left)
        256u,                        // top right: HL1
        in_top
      );
      let i = base + dy * 16u + dx;
      v = read_coeff(tile_idx, ch, i);
      workArea[workarea_idx(tile_idx, ch, y, x)] = v;
    }
  }
  storageBarrier();
  workgroupBarrier();

  // Each workgroup contributes 16×16. Inverse vertical + horizontal passes
  // need the FULL 32×32. We split the 32×32 inverse across the four
  // workgroups by assigning each one a slice of columns (vertical) and rows
  // (horizontal). Quadrant 0 (top-left) does columns 0..16 vertical AND
  // rows 0..16 horizontal; quadrant 1 (top-right) does columns 16..32
  // vertical AND rows 0..16 horizontal still won't work because the
  // vertical pass needs the whole column.
  //
  // To keep concurrency simple AND correctness intact, we do the inverse
  // passes inside ONLY workgroup 0 of each tile (quadrant == 0), which
  // already has all 32×32 data in workArea after the loads above run for
  // all four workgroups. We need a cross-workgroup barrier — WebGPU only
  // provides storageBarrier within a workgroup, NOT across workgroups in
  // the same dispatch. Solution: do the L1 inverse step as a SECOND
  // compute pass dispatching one workgroup per tile, after the load pass
  // completes. See encodeInverse in cdf53.ts for the orchestration.

  // (No inverse math here — that runs in cdf53_inverse_l1_pass2.wgsl below.)

  // For the LL part (already correctly preserved), still nothing else to
  // do; for the detail part, the read_coeff/store happened above.
  //
  // Final pixel write happens in pass 2 after the inverse.
}
