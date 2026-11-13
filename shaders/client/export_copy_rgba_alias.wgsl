// SPDX-License-Identifier: MIT OR Apache-2.0
// export_copy_rgba_alias.wgsl -- copy the damaged tiles of the framebuffer into
// an export buffer, touching nothing else.
//
// Why a compute kernel when export_blit.wgsl exists: on a tiled GPU (Mali) a
// render pass loads and stores every screen tile inside the bounding box of its
// draws, so two small changes in opposite corners cost the whole frame in
// bandwidth however tight the scissors are. A dispatch writes only the pixels
// it is asked to.
//
// The export is BGRA and wgpu's GLES backend cannot bind that as a storage
// image, so `dst` here is an rgba8unorm ALIAS of the same GL texture
// (see ExportedImage::storage_texture). The image is reinterpreted, not
// converted: component 0 of the stored vec4 lands in byte 0 of the pixel, which
// in the BGRA buffer is blue. Hence the explicit `.bgra` below. The Vulkan
// variant, export_copy_bgra.wgsl, writes the vec4 as is.
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
@group(0) @binding(2) var dst: texture_storage_2d<rgba8unorm, write>;

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
    textureStore(dst, vec2<u32>(x, y), c.bgra);
  }
}
