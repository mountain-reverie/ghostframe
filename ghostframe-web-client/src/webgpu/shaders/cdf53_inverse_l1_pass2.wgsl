// cdf53_inverse_l1_pass2.wgsl — runs the 32×32 inverse lifting (per column,
// per row) and writes reconstructed BGR pixels to the framebuffer.
//
// One workgroup per dirty tile, @workgroup_size(32, 1, 1) = 32 invocations.
// Each invocation handles one column (for the vertical pass) or one row
// (for the horizontal pass).

struct Uniforms {
  cols: u32,
  _pad: vec3<u32>,
};

@group(0) @binding(0) var<storage, read> dirtyTiles: array<u32>;
@group(0) @binding(1) var<storage, read_write> workArea: array<i32>;
@group(0) @binding(2) var framebuffer: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(3) var<uniform> uniforms: Uniforms;

fn workarea_idx(tile_idx: u32, ch: u32, y: u32, x: u32) -> u32 {
  return tile_idx * 3072u + ch * 1024u + y * 32u + x;
}

@compute @workgroup_size(32, 1, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>,
        @builtin(local_invocation_index) lid: u32) {
  let tile_idx = dirtyTiles[wg.x];

  // Inverse vertical pass — each invocation handles ONE column.
  for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
    let col = lid;
    var tmp: array<i32, 32>;
    for (var k: u32 = 0u; k < 16u; k = k + 1u) {
      tmp[2u * k] = workArea[workarea_idx(tile_idx, ch, k, col)];
      tmp[2u * k + 1u] = workArea[workarea_idx(tile_idx, ch, 16u + k, col)];
    }
    for (var k: u32 = 0u; k < 32u; k = k + 2u) {
      let left = select(tmp[k - 1u], tmp[1], k == 0u);
      let right = select(tmp[31], tmp[k + 1u], k + 1u < 32u);
      tmp[k] = tmp[k] - ((left + right + 2) >> 2);
    }
    for (var k: u32 = 1u; k < 32u; k = k + 2u) {
      let left = tmp[k - 1u];
      let right = select(tmp[k - 1u], tmp[k + 1u], k + 1u < 32u);
      tmp[k] = tmp[k] + ((left + right) >> 1);
    }
    for (var k: u32 = 0u; k < 32u; k = k + 1u) {
      workArea[workarea_idx(tile_idx, ch, k, col)] = tmp[k];
    }
  }
  workgroupBarrier();
  storageBarrier();

  // Inverse horizontal pass — each invocation handles ONE row.
  for (var ch: u32 = 0u; ch < 3u; ch = ch + 1u) {
    let row = lid;
    var tmp: array<i32, 32>;
    for (var k: u32 = 0u; k < 16u; k = k + 1u) {
      tmp[2u * k] = workArea[workarea_idx(tile_idx, ch, row, k)];
      tmp[2u * k + 1u] = workArea[workarea_idx(tile_idx, ch, row, 16u + k)];
    }
    for (var k: u32 = 0u; k < 32u; k = k + 2u) {
      let left = select(tmp[k - 1u], tmp[1], k == 0u);
      let right = select(tmp[31], tmp[k + 1u], k + 1u < 32u);
      tmp[k] = tmp[k] - ((left + right + 2) >> 2);
    }
    for (var k: u32 = 1u; k < 32u; k = k + 2u) {
      let left = tmp[k - 1u];
      let right = select(tmp[k - 1u], tmp[k + 1u], k + 1u < 32u);
      tmp[k] = tmp[k] + ((left + right) >> 1);
    }
    for (var k: u32 = 0u; k < 32u; k = k + 1u) {
      workArea[workarea_idx(tile_idx, ch, row, k)] = tmp[k];
    }
  }
  workgroupBarrier();

  // Write pixels. tile_idx → (tile_y, tile_x) using uniforms.cols.
  // Each invocation writes 32 pixels (one row), one channel value at a time.
  let tile_y = tile_idx / uniforms.cols;
  let tile_x = tile_idx % uniforms.cols;
  let row = lid;
  for (var col: u32 = 0u; col < 32u; col = col + 1u) {
    let b = workArea[workarea_idx(tile_idx, 0u, row, col)];
    let g = workArea[workarea_idx(tile_idx, 1u, row, col)];
    let r = workArea[workarea_idx(tile_idx, 2u, row, col)];
    let bf = f32(clamp(b, 0, 255)) / 255.0;
    let gf = f32(clamp(g, 0, 255)) / 255.0;
    let rf = f32(clamp(r, 0, 255)) / 255.0;
    let fb_x = tile_x * 32u + col;
    let fb_y = tile_y * 32u + row;
    textureStore(framebuffer, vec2<u32>(fb_x, fb_y), vec4<f32>(rf, gf, bf, 1.0));
  }
}
