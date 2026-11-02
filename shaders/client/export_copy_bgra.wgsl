// export_copy_bgra.wgsl -- the Vulkan variant of export_copy_rgba_alias.wgsl;
// read that one for why this is a compute kernel at all.
//
// Here the device has BGRA8UNORM_STORAGE and the export is bound as the
// bgra8unorm storage image it really is, so the vec4 is stored as is and the
// API puts it in BGRA byte order, as a render target would. Writing `.bgra`
// here would swap twice.
//
// Same dispatch shape as rgba_tiles.wgsl, and for the same reason (128
// invocations per workgroup, Mali-T860's limit): dispatchWorkgroups(num_tiles,
// 2, 2), each workgroup a 16x16 quarter of the tile, 16x8 invocations, two
// rows each.
//   wg.x = index of the tile in the list
//   wg.y = which 16-row half, wg.z = which 16-column half

// (tile_x, tile_y) per entry, in tile units. Each tile must appear once.
@group(0) @binding(0) var<storage, read> tiles: array<vec2<u32>>;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var dst: texture_storage_2d<bgra8unorm, write>;

@compute @workgroup_size(16, 8, 1)
fn main(
  @builtin(workgroup_id) wg: vec3<u32>,
  @builtin(local_invocation_id) lid: vec3<u32>,
) {
  let tile = tiles[wg.x];
  // Both, so a frame and an export that disagree on size cannot be
  // overrun by either.
  let dims = min(textureDimensions(src), textureDimensions(dst));
  let x = tile.x * 32u + wg.z * 16u + lid.x;
  if (x >= dims.x) {
    return; // the right edge: off the frame for both rows
  }
  for (var yy: u32 = 0u; yy < 2u; yy = yy + 1u) {
    let y = tile.y * 32u + wg.y * 16u + lid.y * 2u + yy;
    if (y >= dims.y) {
      continue;
    }
    let c = textureLoad(src, vec2<u32>(x, y), 0);
    textureStore(dst, vec2<u32>(x, y), c);
  }
}
