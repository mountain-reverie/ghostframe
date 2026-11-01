// Copy already-decoded RGBA tiles into the framebuffer: every tile of a flush
// arrives in ONE storage buffer, and one dispatch scatters them to their
// places, however the dirty tiles are laid out on screen.
//
// Why not write_texture: each call costs ~178us of CPU on a Pinebook Pro
// whatever its size, so a flush of N scattered tiles (text changing across the
// screen, say) cost N calls even after coalescing adjacent tiles into
// rectangles. Here it is one write_buffer and one dispatch.
//
// Same dispatch shape as palrle_decode.wgsl, and for the same reason (128
// invocations per workgroup, Mali-T860's limit): dispatchWorkgroups(num_tiles,
// 2, 2), each workgroup a 16x16 quarter of the tile, 16x8 invocations, two
// rows each.
//   wg.x = index of the tile in this batch
//   wg.y = which 16-row half, wg.z = which 16-column half

struct TileWork {
  tile_x : u32,
  tile_y : u32,
};

@group(0) @binding(0) var<storage, read> tile_work: array<TileWork>;
// 1024 words per tile, row-major, one RGBA8 pixel per word (byte 0 = R).
@group(0) @binding(1) var<storage, read> pixels: array<u32>;
@group(0) @binding(2) var framebuffer: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(16, 8, 1)
fn main(
  @builtin(workgroup_id) wg: vec3<u32>,
  @builtin(local_invocation_id) lid: vec3<u32>,
) {
  let work = tile_work[wg.x];
  let fb_dims = textureDimensions(framebuffer);
  let x_in_tile = wg.z * 16u + lid.x;
  let dst_x = work.tile_x * 32u + x_in_tile;
  if (dst_x >= fb_dims.x) {
    return; // the right edge: this column is off the framebuffer for both rows
  }
  for (var yy: u32 = 0u; yy < 2u; yy = yy + 1u) {
    let y_in_tile = wg.y * 16u + lid.y * 2u + yy;
    let dst_y = work.tile_y * 32u + y_in_tile;
    if (dst_y >= fb_dims.y) {
      continue;
    }
    let word = pixels[wg.x * 1024u + y_in_tile * 32u + x_in_tile];
    textureStore(framebuffer, vec2<u32>(dst_x, dst_y), unpack4x8unorm(word));
  }
}
