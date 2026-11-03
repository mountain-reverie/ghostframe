// cdf53_reconstruct.wgsl -- stage B of the full-grid CDF 5/3 decoder: plane
// store -> pixels, one workgroup per listed tile.
//
// 96 invocations = 3 channels x 32. Invocation (ch, w) owns word `w` of each
// valid plane of channel `ch`, i.e. the 32 layout-order coefficients
// 32*w .. 32*w+31. It assembles sign and magnitude of those 32 coefficients
// in registers, four at a time as vec4s (Mali-T860 issues one scalar op per
// 128-bit ALU instruction, so the same loop in scalars was four times slower),
// applies the midpoint rule, and scatters them into the tile's 32x32 work area
// in workgroup memory (12 KB, under the 16352-byte downlevel limit). The
// pipeline is created without wgpu's zero-fill of workgroup memory, which on
// Mali cost four times the rest of the shader. The inverse lifting then runs
// in that work area, three levels of (columns, rows) with one barrier between
// steps, and the pixels are stored to the framebuffer. Nothing about the tile
// persists between frames: the plane store is the only state, and `plane_mask`
// says which of its planes belong to this generation.
//
// Bit-exact against `cdf53::decode_planes` followed by `cdf53::inverse`.
// `inverse` has no level shift, and the lifting is the closed form of
// `row_inverse`: on a line holding L_0..L_{h-1}, H_0..H_{h-1},
//   L'_k = L_k - ((H_max(k-1,0) + H_k + 2) >> 2)
//   H'_k = H_k + ((L'_k + L'_min(k+1,h-1)) >> 1)
// interleaved as L'_0 H'_0 L'_1 H'_1 ..., where the max/min are the mirror
// boundary of the reference.

@group(0) @binding(0) var<storage, read> planes: array<u32>;
// The frame buffer (see `Cdf53Grid` for the layout). Words used here:
//   [0] tile grid columns   [1] framebuffer width   [2] framebuffer height
//   [5] tile table offset (words)
// Tile table row, 2 words: tile_index, then plane_mask | (K << 16) -- bit p of
// the mask says plane p holds this generation's data (a resolved-but-absent
// plane is simply not set), K is the length of the resolved prefix (1..=14).
// The workgroup count is the tile count; the header does not repeat it.
@group(0) @binding(1) var<storage, read> frame: array<u32>;
@group(0) @binding(2) var framebuffer: texture_storage_2d<rgba8unorm, write>;
// Work-area slot of each of a channel's 1024 coefficients, two u16 to a word
// (low half first), built once by the host from the quadrant layout and
// the placement rule at the end of the lifting comment below (the pipeline
// module's `coefficient_slots`).
@group(0) @binding(3) var<uniform> slots: array<vec4<u32>, 128>;

const H_COLS: u32 = 0u;
const H_FB_WIDTH: u32 = 1u;
const H_FB_HEIGHT: u32 = 2u;
const H_TILES_OFF: u32 = 5u;

const TILE_WORDS: u32 = 1344u;
const PLANE_WORDS: u32 = 96u;

var<workgroup> work: array<i32, 3072>;
// OR of every magnitude word of the tile; decides the "nothing to draw" case.
var<workgroup> any_magnitude: atomic<u32>;
var<workgroup> draw_flag: u32;

// The inverse lifting works in place in the deinterleaved layout, and the
// interleave that `row_inverse` performs is never done in memory. Doing it
// needs a copy of the line (a private array per invocation), and a shader
// that holds two 16-element arrays per invocation did not run at all on
// Mali-T860: the dispatch silently produced nothing once the lifting loop ran
// past its first level.
//
// Instead each level leaves its output where its input was -- L'_k at the
// slot L_k came from, H'_k at the slot of H_k -- and the interleaved (logical)
// position y of a sample is mapped to its slot by phys_N(y), N = the level
// that produced it (N = 1 is the innermost, 8x8):
//   phys_0(y) = y
//   phys_N(y) = y even: phys_{N-1}(y / 2),  y odd: 4 * 2^(N-1) + y / 2
// A lifting line needs the L half through phys_{N-1} and the H half at its
// coefficient-layout position. Both axes are permuted alike, and `place`
// puts the coefficients of the outer bands where the same rule finds them, so
// that one slot function serves every line of a pass. The host applies `place`
// when it builds the slot table: a coefficient at (y, x) of the quadrant layout
// is stored at
//   row    y < 8, 8 <= x < 16 -> phys1(y);   y < 16, x >= 16 -> phys2(y);
//   column 8 <= y < 16, x < 8 -> phys1(x);   y >= 16, x < 16  -> phys2(x);
// and at (y, x) itself otherwise.

fn phys1(y: u32) -> u32 { return (y >> 1u) + ((y & 1u) << 2u); }
fn phys2(y: u32) -> u32 { return select(8u + (y >> 1u), phys1(y >> 1u), (y & 1u) == 0u); }
fn phys3(y: u32) -> u32 { return select(16u + (y >> 1u), phys2(y >> 1u), (y & 1u) == 0u); }

// Slot of element `e` of a level-`level` lifting line (0: 8 samples, 1: 16,
// 2: 32): the L half sits where the previous level left it.
fn line_slot(level: u32, e: u32) -> u32 {
  if (level == 0u || e >= (4u << level)) { return e; }
  if (level == 1u) { return phys1(e); }
  return phys2(e);
}

// One inverse lifting line of 2h samples at work[base + slot * stride]: the
// update and predict steps fused, streaming over k with L'_k and H_k in
// registers so every sample is read once and written once. L'_{k+1} needs
// H_k and H_{k+1}, H'_k needs L'_k and L'_{k+1}; the mirror at the far end
// reuses L'_{h-1}.
fn inverse_line(base: u32, stride: u32, level: u32) {
  let h = 4u << level;
  var h_cur = work[base + h * stride];
  var s_cur = line_slot(level, 0u);
  // L'_0: the mirror makes its left neighbour H_0 as well.
  var l_cur = work[base + s_cur * stride] - ((h_cur + h_cur + 2) >> 2u);
  for (var k: u32 = 0u; k + 1u < h; k = k + 1u) {
    let h_next = work[base + (h + k + 1u) * stride];
    let s_next = line_slot(level, k + 1u);
    let l_next = work[base + s_next * stride] - ((h_cur + h_next + 2) >> 2u);
    work[base + s_cur * stride] = l_cur;
    work[base + (h + k) * stride] = h_cur + ((l_cur + l_next) >> 1u);
    h_cur = h_next;
    l_cur = l_next;
    s_cur = s_next;
  }
  work[base + s_cur * stride] = l_cur;
  work[base + (2u * h - 1u) * stride] = h_cur + l_cur;
}

@compute @workgroup_size(96, 1, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>,
        @builtin(local_invocation_index) lid: u32) {
  let row = frame[H_TILES_OFF] + wg.x * 2u;
  let tile = frame[row];
  let k_prefix = min(frame[row + 1u] >> 16u, 14u);
  // Both checks are uniform across the workgroup (they read only the row). The
  // tile is compared before it is multiplied by the slot size so a wild index
  // cannot wrap into range.
  if (k_prefix == 0u || tile >= arrayLength(&planes) / TILE_WORDS) { return; }
  // Bits beyond the prefix are not data of this prefix, whatever the host set.
  let mask = frame[row + 1u] & ((1u << k_prefix) - 1u) & 0x3FFFu;

  let ch = lid >> 5u;
  let w = lid & 31u;

  // The pipeline is built without wgpu's zero-fill of workgroup memory (it is
  // most of this shader's runtime on Mali), so nothing here starts at zero.
  // `work` is written whole before it is read; the flag is set up by hand, and
  // the barrier keeps invocation 0's store ahead of everyone's atomicOr.
  if (lid == 0u) { atomicStore(&any_magnitude, 0u); }
  workgroupBarrier();

  // Bit-planes of this word: p0 = signs, p1..p13 = magnitude bits 12..0.
  var mag_or = 0u;
  let word_base = tile * TILE_WORDS + ch * 32u + w;
  var p0: u32 = 0u;
  if ((mask & 1u) != 0u) { p0 = planes[word_base + 0u]; }
  var p1: u32 = 0u;
  if ((mask & 2u) != 0u) { p1 = planes[word_base + 96u]; mag_or = mag_or | p1; }
  var p2: u32 = 0u;
  if ((mask & 4u) != 0u) { p2 = planes[word_base + 192u]; mag_or = mag_or | p2; }
  var p3: u32 = 0u;
  if ((mask & 8u) != 0u) { p3 = planes[word_base + 288u]; mag_or = mag_or | p3; }
  var p4: u32 = 0u;
  if ((mask & 16u) != 0u) { p4 = planes[word_base + 384u]; mag_or = mag_or | p4; }
  var p5: u32 = 0u;
  if ((mask & 32u) != 0u) { p5 = planes[word_base + 480u]; mag_or = mag_or | p5; }
  var p6: u32 = 0u;
  if ((mask & 64u) != 0u) { p6 = planes[word_base + 576u]; mag_or = mag_or | p6; }
  var p7: u32 = 0u;
  if ((mask & 128u) != 0u) { p7 = planes[word_base + 672u]; mag_or = mag_or | p7; }
  var p8: u32 = 0u;
  if ((mask & 256u) != 0u) { p8 = planes[word_base + 768u]; mag_or = mag_or | p8; }
  var p9: u32 = 0u;
  if ((mask & 512u) != 0u) { p9 = planes[word_base + 864u]; mag_or = mag_or | p9; }
  var p10: u32 = 0u;
  if ((mask & 1024u) != 0u) { p10 = planes[word_base + 960u]; mag_or = mag_or | p10; }
  var p11: u32 = 0u;
  if ((mask & 2048u) != 0u) { p11 = planes[word_base + 1056u]; mag_or = mag_or | p11; }
  var p12: u32 = 0u;
  if ((mask & 4096u) != 0u) { p12 = planes[word_base + 1152u]; mag_or = mag_or | p12; }
  var p13: u32 = 0u;
  if ((mask & 8192u) != 0u) { p13 = planes[word_base + 1248u]; mag_or = mag_or | p13; }

  var midpoint = 0u;
  if (k_prefix >= 2u && k_prefix < 14u) { midpoint = 1u << (13u - k_prefix); }
  let one = vec4<u32>(1u);
  let wbase = ch * 1024u;
  // Four coefficients at a time, as one vec4 per operation.
  //
  // The bound is 8 in every case: the header's word 15 is always zero. The
  // compiler cannot know that, so it does not unroll the loop, and Mali's
  // register allocator copes with the rolled form (instructions 1743 -> 905
  // and spills 163 -> 56 against the unrolled one, measured with shaderdb); the
  // unrolled form is also what tips it over into silently writing nothing.
  let ngroups = 8u + (frame[15] >> 31u);
  for (var g: u32 = 0u; g < ngroups; g = g + 1u) {
    let sh = vec4<u32>(0u, 1u, 2u, 3u) + vec4<u32>(g * 4u);
    var mag = vec4<u32>(0u);
    mag = mag | (((vec4<u32>(p1) >> sh) & one) << vec4<u32>(12u));
    mag = mag | (((vec4<u32>(p2) >> sh) & one) << vec4<u32>(11u));
    mag = mag | (((vec4<u32>(p3) >> sh) & one) << vec4<u32>(10u));
    mag = mag | (((vec4<u32>(p4) >> sh) & one) << vec4<u32>(9u));
    mag = mag | (((vec4<u32>(p5) >> sh) & one) << vec4<u32>(8u));
    mag = mag | (((vec4<u32>(p6) >> sh) & one) << vec4<u32>(7u));
    mag = mag | (((vec4<u32>(p7) >> sh) & one) << vec4<u32>(6u));
    mag = mag | (((vec4<u32>(p8) >> sh) & one) << vec4<u32>(5u));
    mag = mag | (((vec4<u32>(p9) >> sh) & one) << vec4<u32>(4u));
    mag = mag | (((vec4<u32>(p10) >> sh) & one) << vec4<u32>(3u));
    mag = mag | (((vec4<u32>(p11) >> sh) & one) << vec4<u32>(2u));
    mag = mag | (((vec4<u32>(p12) >> sh) & one) << vec4<u32>(1u));
    mag = mag | (((vec4<u32>(p13) >> sh) & one) << vec4<u32>(0u));
    let neg = ((vec4<u32>(p0) >> sh) & one) != vec4<u32>(0u);
    mag = mag + select(vec4<u32>(0u), vec4<u32>(midpoint), mag != vec4<u32>(0u));
    let val = select(vec4<i32>(mag), -vec4<i32>(mag), neg);
    let quad = slots[w * 4u + (g >> 1u)];
    let pair = select(quad.xy, quad.zw, (g & 1u) == 1u);
    work[wbase + (pair.x & 0xFFFFu)] = val.x;
    work[wbase + (pair.x >> 16u)] = val.y;
    work[wbase + (pair.y & 0xFFFFu)] = val.z;
    work[wbase + (pair.y >> 16u)] = val.w;
  }

  if (k_prefix < 14u && mag_or != 0u) { atomicOr(&any_magnitude, 1u); }
  workgroupBarrier();
  if (lid == 0u) { draw_flag = select(0u, 1u, k_prefix >= 14u || atomicLoad(&any_magnitude) != 0u); }
  workgroupBarrier();
  // A prefix with no magnitude bit reconstructs as black, which is not a
  // picture; the tile keeps what the framebuffer shows.
  if (workgroupUniformLoad(&draw_flag) == 0u) { return; }

  let lane = w;
  let cbase = ch * 1024u;
  if (lane < 8u) { inverse_line(cbase + lane, 32u, 0u); }
  workgroupBarrier();
  if (lane < 8u) { inverse_line(cbase + lane * 32u, 1u, 0u); }
  workgroupBarrier();
  if (lane < 16u) { inverse_line(cbase + lane, 32u, 1u); }
  workgroupBarrier();
  if (lane < 16u) { inverse_line(cbase + lane * 32u, 1u, 1u); }
  workgroupBarrier();
  inverse_line(cbase + lane, 32u, 2u);
  workgroupBarrier();
  inverse_line(cbase + lane * 32u, 1u, 2u);
  workgroupBarrier();

  // 96 divides evenly into rows of 32, so each invocation keeps one column.
  let col = lid & 31u;
  let col_slot = phys3(col);
  let cols = frame[H_COLS];
  let tile_x = (tile % cols) * 32u;
  let tile_y = (tile / cols) * 32u;
  let px = tile_x + col;
  for (var y: u32 = lid >> 5u; y < 32u; y = y + 3u) {
    let py = tile_y + y;
    if (px < frame[H_FB_WIDTH] && py < frame[H_FB_HEIGHT]) {
      let slot = phys3(y) * 32u + col_slot;
      let bgr = clamp(vec3<i32>(work[slot], work[1024u + slot], work[2048u + slot]), vec3<i32>(0), vec3<i32>(255));
      let rgb = vec3<f32>(bgr.zyx) * (1.0 / 255.0);
      textureStore(framebuffer, vec2<u32>(px, py), vec4<f32>(rgb, 1.0));
    }
  }
}
